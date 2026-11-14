use super::*;
use crate::frames::FrameWriter;

fn temp_store(name: &str) -> (PathBuf, Store) {
    let dir = std::env::temp_dir().join(format!("testudo-store-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::open(&dir).unwrap();
    (dir, store)
}

fn add_run(store: &Store, id: &str, started: f64, frame_bytes: usize) {
    store
        .conn
        .execute(
            "INSERT INTO runs (id, started_at, status) VALUES (?1, ?2, 'passed')",
            params![id, started],
        )
        .unwrap();
    let mut w = FrameWriter::create(&store.frame_path(id, "p")).unwrap();
    w.append(started, &vec![started as u8; frame_bytes]).unwrap();
    w.flush().unwrap();
}

#[test]
fn prune_by_count_deletes_oldest_runs() {
    let (dir, store) = temp_store("count");
    for i in 0..5 {
        add_run(&store, &format!("r{i}"), i as f64, 10);
    }
    let r = store
        .prune(&Retention {
            keep_runs: 3,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(r.runs_deleted, 2);
    let ids: Vec<String> = store.list_runs(10).unwrap().into_iter().map(|r| r.id).collect();
    assert_eq!(ids, ["r4", "r3", "r2"]);
    assert!(!store.frame_path("r0", "p").exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn size_cap_drops_oldest_frames_but_keeps_history() {
    let (dir, store) = temp_store("size");
    for i in 0..4 {
        add_run(&store, &format!("r{i}"), i as f64, 1000);
    }
    let r = store
        .prune(&Retention {
            max_frame_bytes: 2500,
            ..Default::default()
        })
        .unwrap();
    assert_eq!((r.runs_deleted, r.recordings_deleted), (0, 2));
    assert!(store.frame_path("r3", "p").exists());
    assert!(store.frame_path("r2", "p").exists());
    assert!(!store.frame_path("r1", "p").exists());
    assert_eq!(store.list_runs(10).unwrap().len(), 4, "history must survive");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn recorder_persists_events_frames_and_summary() {
    use testudo_core::Hub;
    use testudo_protocol::{Event, FrameHeader, Message, TestStatus};

    let (dir, _) = temp_store("recorder");
    let hub = Hub::default();
    let rec = Recorder::spawn(&dir, &hub, Retention::default()).unwrap();
    let ev = |e: Event| Message::Event(e);
    hub.publish(
        "r1",
        ev(Event::RunStarted {
            label: "demo".into(),
            runner: "playwright".into(),
            cwd: "/x".into(),
            args: vec![],
        }),
    );
    hub.publish(
        "r1",
        ev(Event::TestBegin {
            test_id: "t1".into(),
            title: "works".into(),
            title_path: vec!["a.spec.ts".into(), "works".into()],
            file: Some("a.spec.ts".into()),
            line: Some(1),
            project: None,
            worker_index: Some(0),
        }),
    );
    hub.publish(
        "r1",
        ev(Event::PageOpened {
            page_id: "p1".into(),
            test_id: Some("t1".into()),
            worker_index: Some(0),
            browser: Some("chromium".into()),
            url: None,
        }),
    );
    for jpeg in [b"one".as_slice(), b"two", b"one"] {
        hub.publish(
            "r1",
            Message::Frame {
                header: FrameHeader {
                    page_id: "p1".into(),
                    ts: 0.0,
                    viewport_width: None,
                    viewport_height: None,
                },
                jpeg: bytes_of(jpeg),
            },
        );
    }
    hub.publish(
        "r1",
        ev(Event::TestEnd {
            test_id: "t1".into(),
            status: TestStatus::Failed,
            duration_ms: Some(5.0),
            error: Some("boom".into()),
        }),
    );
    hub.publish("r1", ev(Event::ProcessExited { code: Some(1) }));
    hub.close();
    rec.join();

    let store = Store::open(&dir).unwrap();
    let run = store.run("r1").unwrap().unwrap();
    assert_eq!((run.label.as_str(), run.status.as_str(), run.failed), ("demo", "failed", 1));
    assert!(run.ended_at.is_some());
    assert_eq!(store.run_events("r1").unwrap().len(), 5);
    let pages = store.run_pages("r1").unwrap();
    assert_eq!((pages[0].frames, pages[0].test_id.as_deref()), (3, Some("t1")));
    let mut idx = crate::frames::FrameIndex::open(&store.frame_path("r1", "p1")).unwrap();
    assert_eq!(idx.read(2).unwrap(), b"one");
    let hist = store.test_history("a.spec.ts", &["a.spec.ts".into(), "works".into()], 5).unwrap();
    assert_eq!(hist[0].status.as_deref(), Some("failed"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn orphaned_running_run_is_marked_interrupted() {
    let (dir, store) = temp_store("orphan");
    // pid that cannot be alive
    store
        .conn
        .execute(
            "INSERT INTO runs (id, started_at, status, owner_pid) VALUES ('r1', 1.0, 'running', 2147483646)",
            [],
        )
        .unwrap();
    drop(store);
    let hub = testudo_core::Hub::default();
    let rec = Recorder::spawn(&dir, &hub, Retention::default()).unwrap();
    hub.close();
    rec.join();
    let run = Store::open(&dir).unwrap().run("r1").unwrap().unwrap();
    assert_eq!(run.status, "interrupted");
    assert_eq!(run.ended_at, Some(1.0));
    std::fs::remove_dir_all(dir).unwrap();
}

fn bytes_of(b: &[u8]) -> testudo_protocol::Bytes {
    testudo_protocol::Bytes::copy_from_slice(b)
}

#[test]
fn late_test_result_after_exit_still_counts() {
    use testudo_core::Hub;
    use testudo_protocol::{Event, Message, TestStatus};

    let (dir, _) = temp_store("late");
    let hub = Hub::default();
    let rec = Recorder::spawn(&dir, &hub, Retention::default()).unwrap();
    let begin = |id: &str| Event::TestBegin {
        test_id: id.into(),
        title: id.into(),
        title_path: vec![],
        file: None,
        line: None,
        project: None,
        worker_index: None,
    };
    let end = |id: &str| Event::TestEnd {
        test_id: id.into(),
        status: TestStatus::Passed,
        duration_ms: None,
        error: None,
    };
    for e in [begin("a"), begin("b"), end("a"), Event::ProcessExited { code: Some(0) }, end("b")] {
        hub.publish("r1", Message::Event(e));
    }
    hub.close();
    rec.join();
    assert_eq!(Store::open(&dir).unwrap().run("r1").unwrap().unwrap().passed, 2);
    std::fs::remove_dir_all(dir).unwrap();
}
