//! How fast the sending side can seal and queue file chunks, with the network taken out (the writer's
//! socket discards everything). Compare thread counts with DRIFT_SEAL_THREADS=1 / 2 / 4.
//!
//!     cargo run --release -p drift-net --example writer_bench

use std::sync::Arc;
use std::time::Instant;

use drift_core::payload;
use drift_core::secure;
use drift_core::ukey2::{ClientHandshake, Role, ServerHandshake};

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let (c, ci) = ClientHandshake::new();
    let (s, si) = ServerHandshake::handle_client_init(&ci).unwrap();
    let (_cr, cf) = c.handle_server_init(&si).unwrap();
    let sr = s.handle_client_finish(&cf).unwrap();
    let (sealer, _) = secure::split(&sr.session_keys(Role::Server));

    let (writer, task) = drift_net::channel::spawn_writer(Box::new(tokio::io::sink()), sealer);
    let window = Arc::new(tokio::sync::Semaphore::new(8));
    let chunk = vec![7u8; 512 * 1024];
    let n = 1024; // 512 MiB
    let t = Instant::now();
    for i in 0..n {
        let f = payload::file_chunk(1, (n * chunk.len()) as i64, "bench.bin", (i * chunk.len()) as i64, chunk.clone(), i + 1 == n);
        if !writer.send_data(f, &window).await {
            break;
        }
    }
    drop(writer);
    let _ = task.await;
    let secs = t.elapsed().as_secs_f64();
    let threads = std::env::var("DRIFT_SEAL_THREADS").unwrap_or_else(|_| "auto".into());
    println!("writer ({threads} seal threads): {} MiB in {secs:.2} s = {:.0} MB/s", n / 2, (n / 2) as f64 / secs);
}
