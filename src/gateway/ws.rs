use std::env::consts;
use std::io::Read;
use std::time::SystemTime;

use flate2::read::ZlibDecoder;
#[cfg(feature = "transport_compression_zlib")]
use flate2::Decompress;
use futures::{SinkExt, StreamExt};
use small_fixed_array::FixedString;
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{connect_async_with_config, MaybeTlsStream, WebSocketStream};
use tracing::{debug, trace, warn};
use url::Url;

use super::{ActivityData, ChunkGuildFilter, GatewayError, PresenceData, TransportCompression};
use crate::constants::{self, Opcode};
use crate::model::event::GatewayEvent;
use crate::model::gateway::{GatewayIntents, ShardInfo};
use crate::model::id::{GuildId, UserId};
use crate::{Error, Result};

#[derive(Serialize)]
struct IdentifyProperties {
    browser: &'static str,
    device: &'static str,
    os: &'static str,
}

#[derive(Serialize)]
struct ChunkGuildMessage<'a> {
    guild_id: GuildId,
    #[serde(skip_serializing_if = "Option::is_none")]
    query: Option<&'a str>,
    limit: u16,
    presences: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_ids: Option<Vec<UserId>>,
    nonce: &'a str,
}

#[derive(Serialize)]
struct PresenceUpdateMessage<'a> {
    afk: bool,
    status: &'a str,
    since: SystemTime,
    activities: &'a [ActivityData],
}

#[derive(Serialize)]
#[serde(untagged)]
enum WebSocketMessageData<'a> {
    Heartbeat(Option<u64>),
    ChunkGuild(ChunkGuildMessage<'a>),
    Identify {
        compress: bool,
        token: &'a str,
        large_threshold: u8,
        shard: &'a ShardInfo,
        intents: GatewayIntents,
        properties: IdentifyProperties,
        presence: PresenceUpdateMessage<'a>,
    },
    PresenceUpdate(PresenceUpdateMessage<'a>),
    Resume {
        session_id: &'a str,
        token: &'a str,
        seq: u64,
    },
}

#[derive(Serialize)]
struct WebSocketMessage<'a> {
    op: Opcode,
    d: WebSocketMessageData<'a>,
}

enum Compression {
    Payload {
        decompressed: Vec<u8>,
    },

    #[cfg(feature = "transport_compression_zlib")]
    Zlib {
        inflater: Decompress,
        compressed: Vec<u8>,
        decompressed: Vec<u8>,
    },
}

const DECOMPRESSION_MULTIPLIER: usize = 3;
#[cfg(feature = "transport_compression_zlib")]
const ZLIB_SUFFIX: [u8; 4] = [0x00, 0x00, 0xFF, 0xFF];
#[cfg(feature = "transport_compression_zlib")]
const ZLIB_BUFFER_SIZE: usize = 32 * 1024;

impl Compression {
    fn inflate(&mut self, slice: &[u8]) -> Result<Option<&[u8]>> {
        match self {
            Compression::Payload {
                decompressed,
            } => {
                decompressed.clear();
                decompressed.reserve(slice.len() * DECOMPRESSION_MULTIPLIER);

                ZlibDecoder::new(slice).read_to_end(decompressed).map_err(|why| {
                    warn!("Err decompressing bytes: {why:?}");
                    debug!("Failing bytes: {slice:?}");

                    why
                })?;

                Ok(Some(decompressed.as_slice()))
            },

            #[cfg(feature = "transport_compression_zlib")]
            Compression::Zlib {
                inflater,
                compressed,
                decompressed,
            } => {
                compressed.extend_from_slice(slice);
                let length = compressed.len();

                if length < 4 || compressed[length - 4..] != ZLIB_SUFFIX {
                    return Ok(None);
                }

                let pre_out = inflater.total_out();
                decompressed.clear();
                inflater.decompress_vec(compressed, decompressed, flate2::FlushDecompress::Sync)?;

                let size = inflater.total_out() - pre_out;
                Ok(Some(&decompressed[..size as usize]))
            },
        }
    }
}

impl From<TransportCompression> for Compression {
    fn from(value: TransportCompression) -> Self {
        match value {
            TransportCompression::None => Compression::Payload {
                decompressed: Vec::new(),
            },

            #[cfg(feature = "transport_compression_zlib")]
            TransportCompression::Zlib => Compression::Zlib {
                inflater: Decompress::new(true),
                compressed: Vec::new(),
                decompressed: Vec::with_capacity(ZLIB_BUFFER_SIZE),
            },
        }
    }
}

pub struct WsClient {
    stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    compression: Compression,
}

const TIMEOUT: Duration = Duration::from_millis(500);

impl WsClient {
    pub(crate) async fn connect(url: Url, compression: TransportCompression) -> Result<Self> {
        let config = WebSocketConfig {
            max_message_size: None,
            max_frame_size: None,
            ..Default::default()
        };
        let (stream, _) = connect_async_with_config(url, Some(config), false).await?;

        Ok(Self {
            stream,
            compression: compression.into(),
        })
    }

    pub(crate) async fn recv_json(&mut self) -> Result<Option<GatewayEvent>> {
        let message = match timeout(TIMEOUT, self.stream.next()).await {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(e))) => return Err(e.into()),
            Ok(None) | Err(_) => return Ok(None),
        };

        let json_str = match message {
            Message::Text(payload) => payload,
            Message::Binary(bytes) => {
                let Some(decompressed) = self.compression.inflate(&bytes)? else {
                    return Ok(None);
                };

                String::from_utf8_lossy(decompressed).to_string()
            },
            Message::Close(Some(frame)) => {
                return Err(Error::Gateway(GatewayError::Closed(Some(frame))));
            },
            _ => return Ok(None),
        };

        match serde_json::from_str(&json_str) {
            Ok(mut event) => {
                if let GatewayEvent::Dispatch {
                    original_str, ..
                } = &mut event
                {
                    *original_str = FixedString::from_string_trunc(json_str);
                }

                Ok(Some(event))
            },
            Err(err) => {
                debug!("Failing text: {json_str}");
                Err(Error::Json(err))
            },
        }
    }

    pub(crate) async fn send_json(&mut self, value: &impl serde::Serialize) -> Result<()> {
        let message = serde_json::to_string(value).map(Message::Text)?;

        self.stream.send(message).await?;
        Ok(())
    }

    /// Delegate to `StreamExt::next`
    pub(crate) async fn next(&mut self) -> Option<std::result::Result<Message, WsError>> {
        self.stream.next().await
    }

    /// Delegate to `SinkExt::send`
    pub(crate) async fn send(&mut self, message: Message) -> Result<()> {
        self.stream.send(message).await?;
        Ok(())
    }

    /// Delegate to `WebSocketStream::close`
    pub(crate) async fn close(&mut self, msg: Option<CloseFrame<'_>>) -> Result<()> {
        self.stream.close(msg).await?;
        Ok(())
    }

    #[expect(clippy::missing_errors_doc)]
    pub async fn send_chunk_guild(
        &mut self,
        guild_id: GuildId,
        shard_info: &ShardInfo,
        limit: Option<u16>,
        presences: bool,
        filter: ChunkGuildFilter,
        nonce: Option<&str>,
    ) -> Result<()> {
        debug!("[{:?}] Requesting member chunks", shard_info);

        let (query, user_ids) = match filter {
            ChunkGuildFilter::None => (Some(String::new()), None),
            ChunkGuildFilter::Query(query) => (Some(query), None),
            ChunkGuildFilter::UserIds(user_ids) => (None, Some(user_ids)),
        };

        self.send_json(&WebSocketMessage {
            op: Opcode::RequestGuildMembers,
            d: WebSocketMessageData::ChunkGuild(ChunkGuildMessage {
                guild_id,
                query: query.as_deref(),
                limit: limit.unwrap_or(0),
                presences,
                user_ids,
                nonce: nonce.unwrap_or(""),
            }),
        })
        .await
    }

    /// # Errors
    ///
    /// Errors if there is a problem with the WS connection.
    #[cfg_attr(feature = "tracing_instrument", instrument(skip(self)))]
    pub async fn send_heartbeat(&mut self, shard_info: &ShardInfo, seq: Option<u64>) -> Result<()> {
        trace!("[{:?}] Sending heartbeat d: {:?}", shard_info, seq);

        self.send_json(&WebSocketMessage {
            op: Opcode::Heartbeat,
            d: WebSocketMessageData::Heartbeat(seq),
        })
        .await
    }

    /// # Errors
    ///
    /// Errors if there is a problem with the WS connection.
    #[cfg_attr(feature = "tracing_instrument", instrument(skip(self, token)))]
    pub async fn send_identify(
        &mut self,
        shard: &ShardInfo,
        token: &str,
        intents: GatewayIntents,
        presence: &PresenceData,
    ) -> Result<()> {
        let now = SystemTime::now();
        let activities = presence.activity.as_ref().map(std::slice::from_ref).unwrap_or_default();

        debug!("[{:?}] Identifying", shard);

        let msg = WebSocketMessage {
            op: Opcode::Identify,
            d: WebSocketMessageData::Identify {
                token,
                shard,
                intents,
                compress: matches!(self.compression, Compression::Payload { .. }),
                large_threshold: constants::LARGE_THRESHOLD,
                properties: IdentifyProperties {
                    browser: "serenity",
                    device: "serenity",
                    os: consts::OS,
                },
                presence: PresenceUpdateMessage {
                    afk: false,
                    since: now,
                    status: presence.status.name(),
                    activities,
                },
            },
        };

        self.send_json(&msg).await
    }

    /// # Errors
    ///
    /// Errors if there is a problem with the WS connection.
    #[cfg_attr(feature = "tracing_instrument", instrument(skip(self)))]
    pub async fn send_presence_update(
        &mut self,
        shard_info: &ShardInfo,
        presence: &PresenceData,
    ) -> Result<()> {
        let now = SystemTime::now();
        let activities = presence.activity.as_ref().map(std::slice::from_ref).unwrap_or_default();

        debug!("[{shard_info:?}] Sending presence update");

        self.send_json(&WebSocketMessage {
            op: Opcode::PresenceUpdate,
            d: WebSocketMessageData::PresenceUpdate(PresenceUpdateMessage {
                afk: false,
                since: now,
                activities,
                status: presence.status.name(),
            }),
        })
        .await
    }

    /// # Errors
    ///
    /// Errors if there is a problem with the WS connection.
    #[cfg_attr(feature = "tracing_instrument", instrument(skip(self, token)))]
    pub async fn send_resume(
        &mut self,
        shard_info: &ShardInfo,
        session_id: &str,
        seq: u64,
        token: &str,
    ) -> Result<()> {
        debug!("[{:?}] Sending resume; seq: {}", shard_info, seq);

        self.send_json(&WebSocketMessage {
            op: Opcode::Resume,
            d: WebSocketMessageData::Resume {
                session_id,
                token,
                seq,
            },
        })
        .await
    }
}
