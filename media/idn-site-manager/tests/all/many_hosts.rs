//! Several Site Managers in one process, with IDN Voice, stopped from tokio's blocking
//! threads (an embedder's parallel site tests): no thread panics. Voice's tasks held the Site
//! Manager's state, and the last of it, with the MQTT broker's runtime, went on Voice's
//! own threads: "Cannot drop a runtime in a context where blocking is not allowed", 5–6
//! times a run.

use idn_site_manager::host::{Config, start};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[test]
fn several_site_managers_in_one_process_stop_without_a_panic() {
    let panics = Arc::new(AtomicUsize::new(0));
    let seen = panics.clone();
    let before = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        seen.fetch_add(1, Ordering::SeqCst);
        before(info);
    }));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    for round in 0..2 {
        let sites: Vec<_> = (0..5)
            .map(|i| {
                let dir = idn_common::testing::TempDir::new(&format!("idn-sm-many-{round}-{i}"));
                let site = start(
                    Config {
                        dir: dir.to_path_buf(),
                        passphrase: None,
                        discovery_port: idn_common::testing::test_port(),
                        api_port: Some(idn_common::testing::test_port()),
                        announce: false,
                        open_new_site: None,
                        password: Some("a-long-site-password".into()),
                        restored: false,
                        host: None,
                        voice: true,
                    },
                    |_| {},
                )
                .expect("it starts");
                (site, dir)
            })
            .collect();
        // (Voice and the broker busy for a moment.)
        std::thread::sleep(Duration::from_millis(300));
        rt.block_on(async {
            let stops: Vec<_> = sites
                .into_iter()
                .map(|(site, dir)| {
                    tokio::task::spawn_blocking(move || {
                        site.stop();
                        // (Its folder removed only once it has stopped.)
                        drop(dir);
                    })
                })
                .collect();
            for s in stops {
                s.await.unwrap();
            }
        });
    }
    drop(rt);
    // (Threads let go to end by themselves: a moment for any panic to show.)
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(panics.load(Ordering::SeqCst), 0, "a thread panicked");
}
