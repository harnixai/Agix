//! A `sandbox` section in a project's settings file, from the file to the
//! tools: `deny_read` names a directory, and a sandboxed `Bash` command is
//! kept out of it, as is the `Read` tool when the directory is one outside
//! the project that `read_directories` opened — while everything else in
//! either place stays readable.

use blackbox::{LiveConfig, LiveRun, ProviderStub, Reply};
use serde_json::json;

const SECRET: &str = "PROJECT-DENY-READ-SECRET-5d1";
const SHARED_SECRET: &str = "SHARED-DENY-READ-SECRET-77e";
const OPEN: &str = "SHARED-OPEN-FILE-9b4";

fn result_of(
    events: &rpc_client::TurnEvents,
    matching: impl Fn(&serde_json::Value) -> bool,
) -> (String, bool) {
    let at = events
        .tool_uses
        .iter()
        .position(|(_, input)| matching(input))
        .expect("the call was made");
    let (_, out, is_error) = events.tool_results[at].clone();
    (out, is_error)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_projects_deny_read_keeps_read_and_bash_out_of_the_directory() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = LiveRun::builder("deny-read")
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(&stub),
        ))
        .before_start(|world, project| {
            let shared = world.root().join("shared");
            std::fs::create_dir_all(shared.join("secret"))?;
            std::fs::write(shared.join("secret/key.txt"), SHARED_SECRET)?;
            std::fs::write(shared.join("open.txt"), OPEN)?;
            world.write_project_file("deny-read", "secret/key.txt", SECRET)?;
            let mut settings = LiveRun::base_settings();
            settings["permissions"]["allow"] = json!(["Bash"]);
            settings["sandbox"] = json!({
                "read_directories": [shared],
                "deny_read": [shared.join("secret"), project.join("secret")]
            });
            world.write_project_settings("deny-read", &settings)
        })
        .start()
        .await?;
    let shared = run.world.root().join("shared");
    let session = run.session().await?;
    stub.script([
        Reply::calls(
            "r-open",
            "Read",
            json!({ "file_path": shared.join("open.txt").to_string_lossy() }),
        ),
        Reply::calls(
            "r-secret",
            "Read",
            json!({ "file_path": shared.join("secret/key.txt").to_string_lossy() }),
        ),
        Reply::calls(
            "b-cat",
            "Bash",
            json!({ "command": "cat secret/key.txt; echo end" }),
        ),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "read things").await?;

    let (open, open_failed) = result_of(&events, |i| {
        i["file_path"]
            .as_str()
            .is_some_and(|p| p.ends_with("open.txt"))
    });
    assert!(!open_failed && open.contains(OPEN), "{open}");
    let (secret, _) = result_of(&events, |i| {
        i["file_path"]
            .as_str()
            .is_some_and(|p| p.ends_with("secret/key.txt"))
    });
    assert!(
        !secret.contains(SHARED_SECRET),
        "Read returned a denied file: {secret}"
    );
    let (cat, _) = result_of(&events, |i| i["command"].as_str().is_some());
    assert!(!cat.contains(SECRET), "Bash read a denied file: {cat}");
    run.finish().await;
    Ok(())
}
