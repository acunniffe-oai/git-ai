#![cfg(unix)]

use crate::repos::test_repo::TestRepo;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::Duration;

fn run_adapter(source: &str, script: &str) {
    let repo = TestRepo::new();
    let recorder = repo.path().join("record-checkpoint");
    fs::write(&recorder, "#!/usr/bin/env node\nconst fs=require('node:fs');let input='';process.stdin.on('data',c=>input+=c);process.stdin.on('end',()=>{if(input.trim())fs.appendFileSync(process.env.CAPTURE_PATH,input+'\\n')});\n").unwrap();
    fs::set_permissions(&recorder, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        repo.path().join("adapter.ts"),
        source.replace("__GIT_AI_BINARY_PATH__", recorder.to_str().unwrap()),
    )
    .unwrap();
    let capture = repo.path().join("captured.jsonl");
    let driver = format!(
        "import adapter from './adapter.ts';\nimport assert from 'node:assert/strict';\nimport fs from 'node:fs';\nconst cwd=process.cwd();\nconst payloads=()=>fs.readFileSync(process.env.CAPTURE_PATH,'utf8').trim().split('\\n').map(JSON.parse);\n{script}"
    );
    fs::write(repo.path().join("driver.mjs"), driver).unwrap();
    let child = Command::new("node")
        .args(["--experimental-strip-types", "driver.mjs"])
        .current_dir(repo.path())
        .env("HOME", repo.test_home_path())
        .env("CAPTURE_PATH", capture)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Node is required for adapter protocol tests");
    let output = git_ai::process_timeout::collect_child_output_with_timeout(
        child,
        Duration::from_secs(10),
        Duration::from_millis(10),
    );
    assert!(!output.timed_out, "adapter timed out");
    assert!(
        output.status.is_some_and(|status| status.success()),
        "adapter failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn v2_adapter_pi_uses_native_identity_without_reading_session_or_file_contents() {
    run_adapter(
        include_str!("../../agent-support/pi/git-ai.ts"),
        r#"
const handlers={}; adapter({on:(name,fn)=>handlers[name]=fn});
const ctx={cwd,sessionManager:{getSessionId:()=>"raw-pi-session",getSessionFile:()=>{throw Error('transcript access')}}};
await handlers.tool_call({toolName:'write',toolCallId:'write-1',input:{path:'edited.txt',content:'private content'}},ctx);
await handlers.tool_result({toolName:'write',toolCallId:'write-1',isError:false,content:'private content'},ctx);
const events=payloads(); assert.equal(events.length,1); assert.equal(events[0].session_id,'raw-pi-session');
assert.equal(events[0].hook_event_name,'after_edit'); assert.equal(events[0].edited_filepaths.length,1);
for(const key of ['dirty_files','session_path','model','tool_result','tool_input']) assert.equal(key in events[0],false);
await handlers.tool_call({toolName:'bash',toolCallId:'shell-1',input:{command:'write-then-fail'}},ctx);
await handlers.tool_result({toolName:'bash',toolCallId:'shell-1',isError:true},ctx);
assert.deepEqual(payloads().slice(1).map(event=>event.hook_event_name),['before_command','after_command']);
"#,
    );
}

#[test]
fn v2_adapter_amp_preserves_thread_identity_and_only_sends_write_evidence() {
    run_adapter(
        include_str!("../../agent-support/amp/git-ai.ts"),
        r#"
const handlers={}; adapter({on:(name,fn)=>handlers[name]=fn,helpers:{filesModifiedByToolCall:()=>[]}});
const ctx={logger:{log:()=>{}}};
await handlers['tool.call']({thread:{id:'T-original'},toolUseID:'edit-1',tool:'Edit',input:{path:'edited.txt',content:'private content'}},ctx);
await handlers['tool.result']({thread:{id:'T-other'},toolUseID:'edit-1',status:'done'},ctx);
const events=payloads(); assert.equal(events.length,1); assert.equal(events[0].thread_id,'T-original');
assert.equal(events[0].hook_event_name,'PostToolUse'); assert.equal(events[0].edited_filepaths.length,1);
assert.equal('tool_input' in events[0],false);
"#,
    );
}

#[test]
fn v2_adapter_opencode_retains_paths_without_forwarding_file_content() {
    run_adapter(
        include_str!("../../agent-support/opencode/git-ai.ts"),
        r#"
const handlers=await adapter({worktree:cwd,directory:cwd});
const call={callID:'patch-1',sessionID:'raw-opencode',tool:'apply_patch',args:{patch:'*** Begin Patch\n*** Add File: edited.txt\n+private content\n*** End Patch'}};
await handlers['tool.execute.before'](call,{args:call.args});
await handlers['tool.execute.after'](call,{metadata:{files:[{filePath:cwd+'/edited.txt'}]}});
const events=payloads(); assert.equal(events.length,1); assert.equal(events[0].session_id,'raw-opencode');
assert.deepEqual(events[0].tool_input.file_paths,[cwd+'/edited.txt']); assert.equal('patch' in events[0].tool_input,false);
"#,
    );
}
