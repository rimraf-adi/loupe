use std::fs;
use std::io::Write;
use std::time::{Duration, Instant};

use loupe_core::{Config, Error};
use loupe_daemon::{Client, DaemonHandle, DaemonOptions, start};
use loupe_proto::{Request, Response, SearchOptions};

struct Env {
    repo: tempfile::TempDir,
    index: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        let env = Env {
            repo: tempfile::tempdir().unwrap(),
            index: tempfile::tempdir().unwrap(),
        };
        env.write("src/lib.rs", "pub fn original_function() {}\n");
        env.write("README.md", "# readme\n");
        env
    }

    fn config(&self) -> Config {
        let mut c = Config::new(self.repo.path().canonicalize().unwrap());
        c.index_dir = self.index.path().to_path_buf();
        c
    }

    fn write(&self, rel: &str, content: &str) {
        let p = self.repo.path().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn start(&self, watch: bool) -> DaemonHandle {
        start(
            self.config(),
            DaemonOptions {
                watch,
                reconcile_every: Duration::from_secs(3600),
                ..DaemonOptions::default()
            },
        )
        .unwrap()
    }

    fn client(&self) -> Client {
        Client::connect(&self.config().index_dir).unwrap()
    }
}

fn opts() -> SearchOptions {
    SearchOptions {
        max_files: 100,
        timeout: None,
        ..SearchOptions::default()
    }
}

fn search(c: &mut Client, pattern: &str) -> Vec<String> {
    match c
        .call(&Request::Search {
            pattern: pattern.into(),
            word: false,
            opts: opts(),
        })
        .unwrap()
    {
        Response::Search(r) => {
            let mut v: Vec<String> = r.hits.into_iter().map(|h| h.path).collect();
            v.sort();
            v
        }
        other => panic!("unexpected {other:?}"),
    }
}

fn wait_for(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) -> Duration {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f() {
            return start.elapsed();
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("timed out waiting for: {what}");
}

#[test]
fn serves_search_find_and_status() {
    let env = Env::new();
    let _d = env.start(false);
    let mut c = env.client();
    assert_eq!(c.call(&Request::Ping).unwrap(), Response::Pong);
    assert_eq!(search(&mut c, "original_function"), ["src/lib.rs"]);

    let Response::Search(r) = c
        .call(&Request::Search {
            pattern: "original_function".into(),
            word: true,
            opts: opts(),
        })
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(r.total_files, 1);
    assert!(r.snapshot_id > 0, "answers are stamped with a snapshot id");

    let Response::Fuzzy(f) = c
        .call(&Request::FindFiles {
            query: "rdme".into(),
            limit: 3,
        })
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(f[0].text, "README.md");

    let Response::Status(s) = c.call(&Request::Status).unwrap() else {
        panic!()
    };
    assert_eq!(s.files, 2);
    assert_eq!(s.overlay_files, 0);
    assert_eq!(s.pid, std::process::id());
}

#[test]
fn overlay_put_and_remove_are_immediately_visible() {
    let env = Env::new();
    let _d = env.start(false);
    let mut c = env.client();

    c.call(&Request::Put {
        path: "src/lib.rs".into(),
        content: b"pub fn unsaved_buffer() {}\n".to_vec(),
    })
    .unwrap();
    assert_eq!(search(&mut c, "unsaved_buffer"), ["src/lib.rs"]);
    assert!(
        search(&mut c, "original_function").is_empty(),
        "overlay replaces the on-disk version"
    );

    c.call(&Request::Put {
        path: "new/file.rs".into(),
        content: b"fn brand_new() {}\n".to_vec(),
    })
    .unwrap();
    assert_eq!(search(&mut c, "brand_new"), ["new/file.rs"]);

    c.call(&Request::Remove {
        path: "README.md".into(),
    })
    .unwrap();
    assert!(search(&mut c, "readme").is_empty());

    let Response::Status(s) = c.call(&Request::Status).unwrap() else {
        panic!()
    };
    assert_eq!(s.overlay_files, 3);
}

#[test]
fn update_absorbs_disk_changes_and_clears_the_overlay() {
    let env = Env::new();
    let _d = env.start(false);
    let mut c = env.client();

    c.call(&Request::Put {
        path: "src/lib.rs".into(),
        content: b"pub fn buffer_version() {}\n".to_vec(),
    })
    .unwrap();
    env.write("src/extra.rs", "pub fn added_on_disk() {}\n");
    assert!(
        search(&mut c, "added_on_disk").is_empty(),
        "no watcher: invisible until an update"
    );

    let Response::Updated(u) = c
        .call(&Request::Update {
            force_rehash: false,
        })
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(u.added, 1);
    assert_eq!(search(&mut c, "added_on_disk"), ["src/extra.rs"]);
    // The scan saw the file as saved on disk, so the stale overlay buffer is gone.
    assert!(search(&mut c, "buffer_version").is_empty());
    assert_eq!(search(&mut c, "original_function"), ["src/lib.rs"]);
    let Response::Status(s) = c.call(&Request::Status).unwrap() else {
        panic!()
    };
    assert_eq!(s.overlay_files, 0);
}

#[test]
fn watcher_makes_edits_visible_quickly() {
    let env = Env::new();
    let _d = env.start(true);
    let mut c = env.client();
    // Let the watcher finish registering before the first edit.
    std::thread::sleep(Duration::from_millis(300));

    let started = Instant::now();
    env.write("src/watched.rs", "pub fn edited_while_watching() {}\n");
    let latency = wait_for("edit visible", Duration::from_secs(5), || {
        !search(&mut c, "edited_while_watching").is_empty()
    });
    eprintln!(
        "edit -> visible: {:?} (measured from write: {:?})",
        latency,
        started.elapsed()
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "took {:?}",
        started.elapsed()
    );

    // Modify and delete are tracked too.
    env.write("src/watched.rs", "pub fn rewritten_content() {}\n");
    wait_for("modification visible", Duration::from_secs(5), || {
        !search(&mut c, "rewritten_content").is_empty()
    });
    assert!(search(&mut c, "edited_while_watching").is_empty());

    fs::remove_file(env.repo.path().join("src/watched.rs")).unwrap();
    wait_for("deletion visible", Duration::from_secs(5), || {
        search(&mut c, "rewritten_content").is_empty()
    });

    // Ignored and binary files never enter the results.
    env.write(".gitignore", "ignored_dir/\n");
    std::thread::sleep(Duration::from_millis(300));
    env.write("ignored_dir/secret_stuff.rs", "fn should_not_appear() {}\n");
    fs::write(
        env.repo.path().join("blob.bin"),
        [0u8, 1, 2, b'x', b'y', b'z', b'q', b'q'],
    )
    .unwrap();
    env.write("visible_marker.rs", "fn marker_after_ignored() {}\n");
    wait_for("marker visible", Duration::from_secs(5), || {
        !search(&mut c, "marker_after_ignored").is_empty()
    });
    assert!(search(&mut c, "should_not_appear").is_empty());
}

#[test]
fn background_update_eventually_folds_watcher_edits_into_the_snapshot() {
    let env = Env::new();
    let _d = env.start(true);
    let mut c = env.client();
    std::thread::sleep(Duration::from_millis(300));
    let Response::Status(before) = c.call(&Request::Status).unwrap() else {
        panic!()
    };

    env.write("src/folded.rs", "pub fn folded_into_snapshot() {}\n");
    wait_for(
        "snapshot advances and overlay drains",
        Duration::from_secs(10),
        || {
            let Response::Status(s) = c.call(&Request::Status).unwrap() else {
                panic!()
            };
            s.snapshot_id > before.snapshot_id && s.overlay_files == 0
        },
    );
    assert_eq!(search(&mut c, "folded_into_snapshot"), ["src/folded.rs"]);
}

#[test]
fn second_daemon_is_refused_and_restart_reuses_the_index() {
    let env = Env::new();
    // Make files old enough that a restart's scan trusts (size, mtime).
    let old = std::time::SystemTime::now() - Duration::from_secs(3600);
    for rel in ["src/lib.rs", "README.md"] {
        fs::File::options()
            .write(true)
            .open(env.repo.path().join(rel))
            .unwrap()
            .set_modified(old)
            .unwrap();
    }

    let d = env.start(false);
    assert!(matches!(
        start(
            env.config(),
            DaemonOptions {
                watch: false,
                ..DaemonOptions::default()
            }
        ),
        Err(Error::Locked(_))
    ));
    let mut c = env.client();
    let Response::Status(first) = c.call(&Request::Status).unwrap() else {
        panic!()
    };

    assert_eq!(c.call(&Request::Shutdown).unwrap(), Response::Ok);
    d.wait();
    assert!(
        Client::connect(&env.config().index_dir).is_err(),
        "socket is gone"
    );

    let _d2 = env.start(false);
    let mut c = env.client();
    let Response::Status(second) = c.call(&Request::Status).unwrap() else {
        panic!()
    };
    assert_eq!(
        second.snapshot_id, first.snapshot_id,
        "unchanged repo: no new generation"
    );
    assert_eq!(search(&mut c, "original_function"), ["src/lib.rs"]);
}

#[test]
fn bad_requests_do_not_hurt_the_daemon() {
    let env = Env::new();
    let _d = env.start(false);

    // Garbage that is a well-formed frame header but invalid MessagePack.
    let mut stream = env.client().into_stream();
    let mut frame = 4u32.to_le_bytes().to_vec();
    frame.extend_from_slice(&[0xc1; 4]);
    stream.write_all(&frame).unwrap();
    let reply: Option<Response> = loupe_proto::read_frame(&mut stream).unwrap();
    assert!(matches!(reply, Some(Response::Error(_))));

    // An absurd length prefix is rejected without allocating.
    let mut stream = env.client().into_stream();
    stream.write_all(&u32::MAX.to_le_bytes()).unwrap();
    let reply: Option<Response> = loupe_proto::read_frame(&mut stream).unwrap();
    assert!(matches!(reply, Some(Response::Error(_))));

    // Abrupt disconnect mid-frame.
    let mut stream = env.client().into_stream();
    stream.write_all(&[10, 0, 0, 0, 1, 2]).unwrap();
    drop(stream);

    // Semantic errors come back as errors on a still-usable connection.
    let mut c = env.client();
    for bad in ["/etc/passwd", "../escape", "a//b", ""] {
        assert!(
            matches!(
                c.request(&Request::Put {
                    path: bad.into(),
                    content: vec![]
                })
                .unwrap(),
                Response::Error(_)
            ),
            "{bad:?} accepted"
        );
    }
    assert!(matches!(
        c.request(&Request::Search {
            pattern: "(".into(),
            word: false,
            opts: opts()
        })
        .unwrap(),
        Response::Error(_)
    ));
    assert_eq!(c.call(&Request::Ping).unwrap(), Response::Pong);
    assert_eq!(
        search(&mut env.client(), "original_function"),
        ["src/lib.rs"]
    );
}

#[test]
fn many_concurrent_clients() {
    let env = Env::new();
    let _d = env.start(false);
    let sock_dir = env.config().index_dir;
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let dir = sock_dir.clone();
            std::thread::spawn(move || {
                let mut c = Client::connect(&dir).unwrap();
                for _ in 0..25 {
                    assert_eq!(search(&mut c, "original_function"), ["src/lib.rs"]);
                }
                c.call(&Request::Put {
                    path: format!("p{i}.txt"),
                    content: format!("marker_{i}").into_bytes(),
                })
                .unwrap();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let mut c = env.client();
    for i in 0..8 {
        assert_eq!(
            search(&mut c, &format!("marker_{i}")),
            [format!("p{i}.txt")]
        );
    }
}

#[test]
fn timer_reconciliation_catches_changes_without_a_watcher() {
    let env = Env::new();
    let _d = start(
        env.config(),
        DaemonOptions {
            watch: false,
            reconcile_every: Duration::from_millis(200),
            rehash_every: Duration::from_millis(500),
            ..DaemonOptions::default()
        },
    )
    .unwrap();
    let mut c = env.client();
    env.write("src/missed_event.rs", "pub fn missed_by_the_watcher() {}\n");
    wait_for(
        "timer-driven scan picks it up",
        Duration::from_secs(10),
        || !search(&mut c, "missed_by_the_watcher").is_empty(),
    );
    let Response::Status(s) = c.call(&Request::Status).unwrap() else {
        panic!()
    };
    assert!(s.updates >= 1);
}

#[test]
fn symbols_and_outlines_through_the_daemon() {
    let env = Env::new();
    let _d = env.start(false);
    let mut c = env.client();

    let Response::Symbols(hits) = c
        .call(&Request::FindSymbol {
            name: "original_function".into(),
            kind: None,
            limit: 10,
        })
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "src/lib.rs");

    let Response::Outline(Some(o)) = c
        .call(&Request::Outline {
            path: "src/lib.rs".into(),
        })
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(o.symbols[0].name, "original_function");

    // The overlay shows through: an unsaved buffer defines a new symbol.
    c.call(&Request::Put {
        path: "src/lib.rs".into(),
        content: b"pub struct InBuffer;\n".to_vec(),
    })
    .unwrap();
    let Response::Symbols(hits) = c
        .call(&Request::FindSymbol {
            name: "InBuffer".into(),
            kind: Some(loupe_proto::SymbolKind::Struct),
            limit: 10,
        })
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(hits.len(), 1);
    let Response::Symbols(old) = c
        .call(&Request::FindSymbol {
            name: "original_function".into(),
            kind: None,
            limit: 10,
        })
        .unwrap()
    else {
        panic!()
    };
    assert!(old.is_empty());

    assert!(matches!(
        c.request(&Request::Outline {
            path: "../etc/passwd".into()
        })
        .unwrap(),
        Response::Error(_)
    ));
    assert_eq!(
        c.call(&Request::Outline {
            path: "nope.rs".into()
        })
        .unwrap(),
        Response::Outline(None)
    );
}

#[test]
fn graph_queries_are_served_and_follow_updates() {
    let env = Env::new();
    env.write("py/a.py", "import b\n");
    env.write("py/b.py", "x = 1\n");
    env.write("py/test_b.py", "import b\n");
    let _d = env.start(false);
    let mut c = env.client();

    let Response::Related(rel) = c
        .call(&Request::Related {
            path: "py/b.py".into(),
            kinds: vec![],
            hops: 1,
            limit: 10,
        })
        .unwrap()
    else {
        panic!()
    };
    let paths: Vec<&str> = rel.iter().map(|r| r.path.as_str()).collect();
    assert!(
        paths.contains(&"py/a.py") && paths.contains(&"py/test_b.py"),
        "{paths:?}"
    );

    let Response::Impact(imp) = c
        .call(&Request::Impact {
            path: "py/b.py".into(),
            depth: 2,
            limit: 10,
        })
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(imp.len(), 2);

    let Response::Ranked(top) = c.call(&Request::TopRanked { limit: 1 }).unwrap() else {
        panic!()
    };
    assert_eq!(top[0].0, "py/b.py", "the file everyone imports ranks first");

    // A new importer shows up after an update: the cached graph is per snapshot.
    env.write("py/c.py", "import b\n");
    c.call(&Request::Update {
        force_rehash: false,
    })
    .unwrap();
    let Response::Impact(imp) = c
        .call(&Request::Impact {
            path: "py/b.py".into(),
            depth: 1,
            limit: 10,
        })
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(imp.len(), 3, "{imp:?}");

    assert!(matches!(
        c.request(&Request::Related {
            path: "../x".into(),
            kinds: vec![],
            hops: 1,
            limit: 1
        })
        .unwrap(),
        Response::Error(_)
    ));
    assert_eq!(
        c.call(&Request::Impact {
            path: "nope.py".into(),
            depth: 1,
            limit: 1
        })
        .unwrap(),
        Response::Impact(vec![])
    );
}

#[test]
fn repo_map_and_module_summaries_through_the_daemon() {
    let env = Env::new();
    env.write(
        "README.md",
        "# Demo\n\nA tiny project used by daemon tests. Nothing else.\n",
    );
    env.write(
        "src/util.rs",
        "//! Helpers shared by the project.\npub fn helper() {}\n",
    );
    let _d = env.start(false);
    let mut c = env.client();

    let Response::Map(map) = c
        .call(&Request::RepoMap {
            budget: 800,
            focus: vec![],
            name: "demo".into(),
        })
        .unwrap()
    else {
        panic!()
    };
    assert!(map.text.starts_with("# demo — "), "{}", map.text);
    assert!(
        map.text
            .contains("Summary: A tiny project used by daemon tests."),
        "{}",
        map.text
    );
    assert!(map.tokens <= 800);

    let Response::Module(m) = c
        .call(&Request::ModuleSummary { path: "src".into() })
        .unwrap()
    else {
        panic!()
    };
    assert!(
        m.text.starts_with("2 files") || m.text.starts_with("1 file"),
        "generated line for a dir without lib.rs/mod.rs: {}",
        m.text
    );
    assert_eq!(m.path, "src");
    assert!(matches!(
        c.request(&Request::ModuleSummary {
            path: "../x".into()
        })
        .unwrap(),
        Response::Error(_)
    ));
    assert!(matches!(
        c.request(&Request::ModuleSummary {
            path: "no/such".into()
        })
        .unwrap(),
        Response::Error(_)
    ));
}

#[test]
fn queries_stay_correct_while_files_churn_and_snapshots_swap() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let env = Env::new();
    for i in 0..30 {
        env.write(
            &format!("src/f{i}.rs"),
            &format!("pub fn churn_{i}() {{ let marker = {i}; }}\n"),
        );
    }
    let _d = env.start(true);
    let dir = env.config().index_dir;
    let stop = Arc::new(AtomicBool::new(false));

    // Readers hammer every kind of request; none may ever see an error or a malformed answer.
    let readers: Vec<_> = (0..4)
        .map(|r| {
            let (dir, stop) = (dir.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut c = Client::connect(&dir).unwrap();
                let mut n = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    let req = match (n + r) % 5 {
                        0 => Request::Search {
                            pattern: "churn_".into(),
                            word: false,
                            opts: opts(),
                        },
                        1 => Request::FindSymbol {
                            name: "churn_3".into(),
                            kind: None,
                            limit: 5,
                        },
                        2 => Request::FindFiles {
                            query: "f1".into(),
                            limit: 5,
                        },
                        3 => Request::Outline {
                            path: "src/f2.rs".into(),
                        },
                        _ => Request::Status,
                    };
                    match c.request(&req).unwrap() {
                        Response::Error(e) => {
                            // A file vanishing between a snapshot and a read is the only
                            // acceptable "error", and it surfaces as stale/unreadable, not here.
                            panic!("reader {r} got an error for {req:?}: {e}");
                        }
                        Response::Search(res) => assert!(res.snapshot_id > 0),
                        _ => {}
                    }
                    n += 1;
                }
                n
            })
        })
        .collect();

    // The writer churns files and forces updates concurrently with the readers.
    let mut c = env.client();
    for round in 0..25 {
        env.write(
            &format!("src/f{}.rs", round % 30),
            &format!(
                "pub fn churn_{}() {{ let marker = {round}; }}\n",
                round % 30
            ),
        );
        env.write(
            &format!("src/extra{round}.rs"),
            &format!("pub fn churn_extra_{round}() {{}}\n"),
        );
        if round % 4 == 0 {
            std::fs::remove_file(
                env.repo
                    .path()
                    .join(format!("src/f{}.rs", (round + 7) % 30)),
            )
            .ok();
        }
        if round % 2 == 0 {
            c.call(&Request::Update {
                force_rehash: false,
            })
            .unwrap();
        }
    }
    stop.store(true, Ordering::Relaxed);
    let served: u32 = readers
        .into_iter()
        .map(|h| h.join().expect("a reader thread panicked"))
        .sum();
    assert!(served > 100, "readers only managed {served} requests");

    // After the dust settles the index agrees with the disk.
    c.call(&Request::Update { force_rehash: true }).unwrap();
    // Ground truth straight from the disk: files that contain the pattern.
    let on_disk: std::collections::BTreeSet<String> =
        std::fs::read_dir(env.repo.path().join("src"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| std::fs::read_to_string(p).is_ok_and(|c| c.contains("churn_")))
            .map(|p| format!("src/{}", p.file_name().unwrap().to_string_lossy()))
            .collect();
    assert_eq!(
        search(&mut c, "churn_")
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        on_disk
    );
}

fn updates(c: &mut Client) -> u64 {
    let Response::Status(s) = c.call(&Request::Status).unwrap() else {
        panic!()
    };
    s.updates
}

#[test]
fn build_output_does_not_wake_the_scanner_but_source_edits_do() {
    let env = Env::new();
    env.write("Cargo.toml", "[package]\nname = \"x\"\n");
    env.write(".gitignore", "dist/\n");
    env.write(
        "assets/keep.txt",
        "an ordinary directory that exists before the build\n",
    );
    let _d = env.start(true);
    let mut c = env.client();
    let base = updates(&mut c);

    // A "build" writing into excluded and ignored places, plus binary files.
    for i in 0..200 {
        env.write(&format!("target/debug/deps/crate{i}.d"), "dep info");
        env.write(
            &format!("node_modules/pkg{i}/index.js"),
            "module.exports = 1;",
        );
        env.write(&format!("dist/out{i}.js"), "var x = 1;");
        env.write(&format!("assets/lib{i}.rlib"), "not really binary");
    }
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        updates(&mut c),
        base,
        "events under excluded/ignored/binary paths must not trigger re-scans"
    );
    // ... and none of it is searchable.
    assert!(search(&mut c, "module.exports").is_empty());
    assert!(search(&mut c, "dep info").is_empty());

    // A real source edit is visible immediately and does trigger a (paced) re-scan.
    env.write("src/lib.rs", "pub fn brand_new_function() {}\n");
    wait_for("edit visible", Duration::from_secs(5), || {
        search(&mut c, "brand_new_function") == ["src/lib.rs"]
    });
    wait_for("paced update runs", Duration::from_secs(10), || {
        updates(&mut c) > base
    });
}

#[test]
fn bursts_are_paced_to_one_scan_per_interval() {
    let env = Env::new();
    let _d = start(
        env.config(),
        DaemonOptions {
            watch: true,
            reconcile_every: Duration::from_secs(3600),
            min_update_interval: Duration::from_millis(1500),
            ..DaemonOptions::default()
        },
    )
    .unwrap();
    let mut c = env.client();
    let base = updates(&mut c);
    let begin = Instant::now();
    for i in 0..60 {
        env.write(&format!("src/f{i}.rs"), &format!("pub fn f{i}() {{}}\n"));
        std::thread::sleep(Duration::from_millis(20));
    }
    wait_for("burst folded into scans", Duration::from_secs(15), || {
        updates(&mut c) > base
    });
    std::thread::sleep(Duration::from_millis(2500));
    let scans = updates(&mut c) - base;
    let secs = begin.elapsed().as_secs_f64();
    assert!(
        scans as f64 <= secs / 1.4 + 1.0,
        "{scans} scans in {secs:.1}s exceeds the 1.5s pacing"
    );
    assert!(scans < 6, "60 writes produced {scans} scans");
}
