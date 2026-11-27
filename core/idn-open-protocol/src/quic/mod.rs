//! QUIC media transport (spec chapters 2 and 4), on quinn with the NDI stub crypto
//! (`ndi_crypto.rs`), and the shared runtime quinn runs on.

pub mod client;
mod ndi_crypto;
pub mod server;
pub mod stub;
pub mod wire;

/// `len` bytes of a stream, the buffer grown as they come, not made at the length a peer
/// says before a byte of it has arrived (64 MB a stream, many streams at once: gigabytes
/// committed by one hostile peer; bug review 3). `None`: the stream ended or was reset
/// first.
pub(crate) async fn read_len(rs: &mut quinn::RecvStream, len: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(len.min(256 << 10));
    while out.len() < len {
        match rs.read_chunk(len - out.len(), true).await {
            Ok(Some(c)) => out.extend_from_slice(&c.bytes),
            _ => return None,
        }
    }
    Some(out)
}

/// The small private tokio runtime quinn runs on, for both the NDI QUIC profile and
/// secure QUIC (`squic`). The rest of the crate is thread-based and uses blocking and
/// channel interfaces on top.
pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("idn-quic")
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}
