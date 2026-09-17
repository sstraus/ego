#![cfg(windows)]

//! Native Windows contracts for the local listener endpoint (regression case).
//!
//! These tests intentionally use Tokio's public named-pipe client to reach the
//! listener. The owner-only DACL is a security property of the native pipe
//! object, not of this public byte-stream API. The safe `windows-permissions`
//! wrapper reads the descriptor back from each live client handle as a kernel
//! object; this module contains no raw security probe.
//!
//! The safe DACL contract for that host gate is an explicit, protected DACL
//! equivalent to `D:P(A;;GA;;;{current TokenUser SID})`: one allow ACE for the
//! current TokenUser SID, no Everyone/Anonymous or inherited principal, and
//! duplex pipe access created through the dependency's safe listener builder.

use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ego_core::endpoint::{Admitted, PublicBind, Requested, admit};
use ego_listen::{Peer, bind};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use tokio::time::timeout;
use windows_permissions::Sid;
use windows_permissions::constants::{
    AccessRights, AceFlags, AceType, SeObjectType, SecurityInformation,
};
use windows_permissions::utilities::current_process_sid;
use windows_permissions::wrappers::GetSecurityInfo as get_security_info;

const LOCAL_PIPE_PREFIX: &str = "\\\\.\\pipe\\";
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(2);
static NEXT_PIPE_ID: AtomicU64 = AtomicU64::new(0);

fn pipe_name() -> String {
    format!(
        "{LOCAL_PIPE_PREFIX}ego-test-120-{}-{}",
        std::process::id(),
        NEXT_PIPE_ID.fetch_add(1, Ordering::Relaxed)
    )
}

fn local_endpoint(name: &str) -> Admitted {
    admit(
        Requested::Local {
            name: name.to_owned(),
        },
        PublicBind::Refused,
    )
    .expect("the non-empty local pipe name is admitted by endpoint policy")
}

async fn accept_current_user(bound: &ego_listen::Bound) -> ego_listen::Accepted {
    timeout(ACCEPT_TIMEOUT, bound.accept())
        .await
        .expect("the named-pipe accept completed within the deadline")
        .expect("the named-pipe accept succeeded")
        .expect("the current-user client was not refused")
}

fn open_client(name: &str) -> NamedPipeClient {
    ClientOptions::new()
        .open(name)
        .unwrap_or_else(|error| panic!("the current-user named-pipe client opened: {error}"))
}

fn assert_owner_only_pipe_dacl<H: AsRawHandle>(pipe: &H, current_sid: &Sid) {
    // `pipe` is the live first/successor client handle returned by the native
    // named-pipe open. This reads the kernel object, not the SDDL supplied at
    // listener construction time.
    let descriptor = get_security_info(
        pipe,
        SeObjectType::SE_KERNEL_OBJECT,
        SecurityInformation::Dacl,
    )
    .expect("the kernel returned the named-pipe security descriptor");
    let dacl = descriptor
        .dacl()
        .expect("the named-pipe security descriptor has a DACL");
    assert_eq!(dacl.len(), 1, "the named-pipe DACL has exactly one ACE");

    let ace = dacl
        .get_ace(0)
        .expect("the named-pipe DACL's sole ACE is readable");
    assert_eq!(
        ace.ace_type(),
        AceType::ACCESS_ALLOWED_ACE_TYPE,
        "the named-pipe DACL's sole ACE allows access"
    );
    assert_eq!(
        ace.flags(),
        AceFlags::empty(),
        "the named-pipe DACL's sole ACE is not inherited"
    );
    assert_eq!(
        ace.mask(),
        AccessRights::FileAllAccess,
        "the named-pipe DACL maps GENERIC_ALL to exactly FILE_ALL_ACCESS"
    );
    assert_eq!(
        ace.sid(),
        Some(current_sid),
        "the named-pipe DACL's sole ACE targets only the current TokenUser SID"
    );

    let sddl = descriptor
        .as_sddl()
        .expect("the kernel descriptor serializes to SDDL")
        .to_string_lossy()
        .into_owned();
    let expected_dacl = format!("P(A;;FA;;;{current_sid})");
    assert!(
        sddl.split_once("D:")
            .is_some_and(|(_, dacl)| dacl == expected_dacl),
        "the named-pipe DACL is protected and has the exact owner-only SDDL: {sddl}"
    );
}

async fn exchange(
    client: &mut NamedPipeClient,
    stream: &mut ego_listen::Stream,
    request: &[u8],
    response: &[u8],
) {
    client.write_all(request).await.expect("client write");

    let mut received = vec![0; request.len()];
    stream
        .read_exact(&mut received)
        .await
        .expect("listener read");
    assert_eq!(received, request, "the public accepted stream read bytes");

    stream.write_all(response).await.expect("listener write");

    let mut reply = vec![0; response.len()];
    client.read_exact(&mut reply).await.expect("client read");
    assert_eq!(reply, response, "the public accepted stream wrote bytes");
}

#[tokio::test]
async fn local_endpoint_binds_a_native_pipe_and_yields_a_current_user_stream() {
    let name = pipe_name();
    let admitted = local_endpoint(&name);
    let bound = bind(&admitted).await.expect("local named-pipe bind");
    assert_eq!(bound.advertised().expect("advertised pipe name"), name);

    let mut client = open_client(&name);
    let accepted = accept_current_user(&bound).await;
    assert_eq!(accepted.peer, Peer::CurrentUser);
    assert!(
        matches!(&accepted.stream, ego_listen::Stream::Pipe(_)),
        "a Windows local endpoint yields the public named-pipe stream variant"
    );

    let mut stream = accepted.stream;
    exchange(&mut client, &mut stream, b"ping", b"pong").await;
}

#[tokio::test]
async fn first_and_successor_pipe_handles_keep_the_exact_owner_only_dacl() {
    let current_sid = current_process_sid().expect("the current process TokenUser SID");
    let name = pipe_name();
    let admitted = local_endpoint(&name);
    let bound = bind(&admitted).await.expect("local named-pipe bind");
    let mut live_sessions = Vec::new();

    for _ in 0..3 {
        let client = open_client(&name);
        let accepted = accept_current_user(&bound).await;
        assert_eq!(accepted.peer, Peer::CurrentUser);
        assert_owner_only_pipe_dacl(&client, &current_sid);
        live_sessions.push((client, accepted.stream));
    }
}

#[tokio::test]
async fn first_pipe_instance_rejects_a_duplicate_owner_bind() {
    let name = pipe_name();
    let admitted = local_endpoint(&name);
    let first = bind(&admitted).await.expect("first local named-pipe bind");

    let second = bind(&admitted).await;
    let error = second
        .err()
        .expect("a second owner must not replace the first pipe instance");
    let rendered = error.to_string().to_ascii_lowercase();
    assert!(
        rendered.contains("pipe")
            && (rendered.contains("use")
                || rendered.contains("exist")
                || rendered.contains("instance")),
        "duplicate bind must explain the actionable conflict: {error}"
    );

    drop(first);
}

#[tokio::test]
async fn sequential_accepts_publish_the_next_pipe_instance_without_a_gap() {
    let name = pipe_name();
    let admitted = local_endpoint(&name);
    let bound = bind(&admitted).await.expect("local named-pipe bind");
    let mut live_sessions = Vec::new();

    for ordinal in 0..3 {
        // This is deliberately one native open, with no retry on
        // ERROR_FILE_NOT_FOUND or ERROR_PIPE_BUSY. A retry would hide a gap
        // between accepted instances and would not prove the contract.
        let mut client = open_client(&name);
        let accepted = accept_current_user(&bound).await;
        assert_eq!(accepted.peer, Peer::CurrentUser);
        let mut stream = accepted.stream;
        let request = format!("ping-{ordinal}");
        let response = format!("pong-{ordinal}");
        exchange(
            &mut client,
            &mut stream,
            request.as_bytes(),
            response.as_bytes(),
        )
        .await;

        // Keep older instances connected while the next native client opens.
        // This catches a single-instance implementation as well as a listener
        // that replaces its server after returning an accepted stream.
        live_sessions.push((client, stream));
    }
}

#[tokio::test]
async fn invalid_pipe_name_returns_an_actionable_input_error() {
    let invalid_name = format!("{LOCAL_PIPE_PREFIX}ego-test-120-invalid\\segment");
    let admitted = local_endpoint(&invalid_name);
    let result = bind(&admitted).await;
    let error = result
        .err()
        .expect("an invalid native pipe name must refuse to bind");

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    let rendered = error.to_string();
    assert!(
        rendered.contains(&invalid_name),
        "invalid-name refusal must identify the requested pipe: {error}"
    );
}
