use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use ext::host::Registry;
use ext::testing::Rig;

use crate::Shell;
use crate::state::Cwd;

#[test]
fn an_extension_cannot_hold_session_state_across_an_await() {
    let rig = Rig::new("hold", Registry::default().with(&Shell).unwrap());
    let (reader, writer) = (rig.cx(), rig.cx());
    let ws = rig.workspace();
    let sub = ws.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let (paused, resume) = (mpsc::channel(), mpsc::channel::<()>());
    let read = std::thread::spawn(move || {
        rt::block_on(async {
            // What a read yields is owned: no lock outlives the closure.
            let before = reader.state(|cwd: &Cwd| cwd.after(None).map(Path::to_path_buf));
            paused.0.send(()).unwrap();
            while resume.1.try_recv().is_err() {
                rt::Reactor::AsyncIo.sleep(Duration::from_millis(5)).await;
            }
            let after = reader.state(|cwd: &Cwd| cwd.after(None).map(Path::to_path_buf));
            (before, after)
        })
    });
    paused.1.recv().unwrap();
    // A change commits while the reader is suspended at an await.
    rt::block_on(writer.update::<Cwd>(&Cwd::change(&sub, None))).unwrap();
    resume.0.send(()).unwrap();
    let (before, after) = read.join().unwrap();
    assert_eq!((before, after), (None, Some(sub)));
}
