//! Paired UDP isolation against children that do have networking.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;

use ego_core::permit::{BackendIdentity, CanonPath, ExecCaps};

use super::{OfflineProbe, PROBE_TIMEOUT, ProbeSuite};
use crate::exec::backend::{Backend, LaunchPrefix};

/// Claims offline capability while launching the target with no confinement.
struct Networked;

impl Backend for Networked {
    fn identity(&self) -> BackendIdentity {
        BackendIdentity::new("networked-claims-offline", "v1")
    }

    fn maximum_demonstrable_caps(&self) -> ExecCaps {
        ExecCaps::NO_IP_NETWORK
    }

    fn launch_prefix(&self) -> LaunchPrefix {
        LaunchPrefix {
            program: None,
            args_before_target: Vec::new(),
        }
    }
}

/// Exits successfully without running the target: a networked child whose
/// datagram the kernel has not delivered yet.
struct DelayedDelivery;

impl Backend for DelayedDelivery {
    fn identity(&self) -> BackendIdentity {
        BackendIdentity::new("delayed-udp-delivery", "v1")
    }

    fn maximum_demonstrable_caps(&self) -> ExecCaps {
        ExecCaps::NO_IP_NETWORK
    }

    fn launch_prefix(&self) -> LaunchPrefix {
        LaunchPrefix {
            program: Some("/usr/bin/true".into()),
            args_before_target: Vec::new(),
        }
    }
}

fn workspace() -> (tempfile::TempDir, CanonPath) {
    let workspace = tempfile::tempdir().expect("workspace");
    let cwd = CanonPath::new(workspace.path().canonicalize().expect("absolute workspace"));
    (workspace, cwd)
}

fn offline_probe(backend: Arc<dyn Backend>, cwd: &CanonPath) -> OfflineProbe<'_> {
    OfflineProbe {
        backend,
        evidence: super::probe_evidence().expect("evidence"),
        cancel: ego_core::turn::CancelToken::new(),
        limits: crate::exec::ExecLimits::default(),
        cwd,
    }
}

fn receiver(address: IpAddr) -> std::io::Result<(tokio::net::UdpSocket, UdpSocket)> {
    let socket = UdpSocket::bind((address, 0))?;
    socket.set_nonblocking(true)?;
    let late = socket.try_clone()?;
    Ok((tokio::net::UdpSocket::from_std(socket)?, late))
}

/// Binds a receiver whose unconfined control exchange pairs, so a refusal
/// afterwards is attributable to the confined half.
async fn paired_receiver(
    network: &OfflineProbe<'_>,
    suite: &ProbeSuite<'_>,
    address: IpAddr,
    timeout: std::time::Duration,
) -> Option<(tokio::net::UdpSocket, UdpSocket)> {
    let (socket, late) = receiver(address).ok()?;
    network
        .udp_control(&socket, suite, timeout)
        .await
        .then_some((socket, late))
}

// Catches: late local delivery escapes a fixed negative window, so a networked
// confined child is admitted as offline.
#[tokio::test]
async fn paired_udp_refuses_a_networked_child_whose_datagram_arrives_late() {
    let (_workspace, cwd) = workspace();
    let network = offline_probe(Arc::new(DelayedDelivery), &cwd);
    let suite = network.suite("offline.udp-loopback");
    let (socket, late) =
        paired_receiver(&network, &suite, Ipv4Addr::LOCALHOST.into(), PROBE_TIMEOUT)
            .await
            .expect("positive UDP control did not complete");
    let endpoint = late.local_addr().expect("endpoint");

    let admitted = network.udp_isolated(socket, &suite).await;

    // The host has returned its verdict, so any receive window is closed. Only
    // now does the child's datagram reach the host socket.
    deliver_late(&late, endpoint);
    assert!(
        !admitted,
        "a networked child whose datagram reached the host after the window was admitted",
    );
}

fn deliver_late(late: &UdpSocket, endpoint: SocketAddr) {
    let datagram = super::sentinel(endpoint);
    UdpSocket::bind("127.0.0.1:0")
        .and_then(|sender| sender.send_to(datagram.as_bytes(), endpoint))
        .expect("late datagram sent");
    late.set_nonblocking(false).expect("blocking receiver");
    late.set_read_timeout(Some(PROBE_TIMEOUT))
        .expect("receiver deadline");
    let mut received = [0_u8; 128];
    let (len, _) = late
        .recv_from(&mut received)
        .expect("the late datagram reached the host");
    assert_eq!(&received[..len], datagram.as_bytes());
}

// Catches: an unconfined child that claims offline is admitted on either
// address class the production probe pairs.
#[tokio::test]
async fn paired_udp_refuses_a_networked_child_on_loopback_and_non_loopback() {
    let (_workspace, cwd) = workspace();
    let network = offline_probe(Arc::new(Networked), &cwd);
    let loopback = network.suite("offline.udp-loopback");
    let (socket, _) = paired_receiver(
        &network,
        &loopback,
        Ipv4Addr::LOCALHOST.into(),
        PROBE_TIMEOUT,
    )
    .await
    .expect("positive loopback UDP control did not complete");
    assert!(
        !network.udp_isolated(socket, &loopback).await,
        "a networked child was admitted on loopback",
    );
    assert_confined_attempt_refused(&network, "offline.udp-loopback");

    let nonloopback = network.suite("offline.udp-nonloopback");
    let selected = super::select_nonloopback(super::nonloopback_addresses(), |address, timeout| {
        paired_receiver(&network, &nonloopback, address, timeout)
    })
    .await;
    let Some((socket, late)) = selected else {
        eprintln!("note: no non-loopback address pairs on this host; non-loopback case skipped");
        return;
    };
    eprintln!(
        "note: non-loopback case at {}",
        late.local_addr().expect("endpoint")
    );
    assert!(
        !network.udp_isolated(socket, &nonloopback).await,
        "a networked child was admitted on a non-loopback address",
    );
    assert_confined_attempt_refused(&network, "offline.udp-nonloopback");
}

fn transcript(network: &OfflineProbe<'_>) -> String {
    network
        .evidence
        .log
        .snapshot()
        .iter()
        .map(|record| format!("{}={}", record.suite, record.outcome))
        .collect::<Vec<_>>()
        .join("; ")
}

fn assert_confined_attempt_refused(network: &OfflineProbe<'_>, suite: &str) {
    let records = network.evidence.log.snapshot();
    let attempt = records
        .iter()
        .rev()
        .find(|record| record.suite == suite)
        .unwrap_or_else(|| panic!("no confined {suite} attempt in {}", transcript(network)));
    assert_ne!(
        attempt.outcome, "ok",
        "the networked child supplied UDP isolation evidence on {suite}",
    );
}

/// Observes on a real Bubblewrap backend each Linux condition the UDP proof
/// relies on, and that the probe admits both address classes there.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_bwrap_udp_isolation_rests_on_a_private_loopback_only_namespace() {
    use ego_core::permit::{Confinement, RunConfinement};

    let (_workspace, cwd) = workspace();
    let policy = RunConfinement::new(Confinement::Off, true, cwd.clone()).expect("policy");
    let scratch = tempfile::tempdir().expect("scratch");
    let scratch = CanonPath::new(scratch.path().canonicalize().expect("scratch path"));
    let backend = match crate::exec::bwrap::BwrapBackend::prepare(&policy, &scratch, &[]) {
        Ok(backend) => Arc::new(backend),
        Err(error) => {
            eprintln!("note: bwrap unavailable here, Linux observation skipped: {error}");
            return;
        }
    };
    let network = offline_probe(backend.clone(), &cwd);
    let observed = cwd.as_path().join("observed");
    let script = format!(
        "grep '^CapEff:' /proc/self/status > {0}; readlink /proc/self/ns/net >> {0}; \
         tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' ' >> {0}",
        observed.display()
    );
    let suite = network.suite("observe.linux-namespace");
    assert!(
        super::probe(
            backend.clone(),
            network.evidence.clone(),
            network.cancel.clone(),
            network.limits,
            &suite,
            vec!["/bin/sh".into(), "-c".into(), script.into()],
        )
        .await,
        "observation command failed: {}",
        transcript(&network),
    );
    let observed = std::fs::read_to_string(observed).expect("observations");
    eprintln!("observed inside bwrap:\n{observed}");
    let mut lines = observed.lines();
    let capabilities = lines.next().expect("CapEff line");
    assert_eq!(
        u64::from_str_radix(capabilities.trim_start_matches("CapEff:").trim(), 16),
        Ok(0),
        "effective capabilities inside bwrap: {capabilities}",
    );
    let host_namespace = super::linux_network_namespace().expect("host namespace");
    let namespace = lines.next().expect("namespace line");
    eprintln!("host network namespace: {host_namespace}");
    assert_ne!(
        namespace, host_namespace,
        "bwrap shares the host network namespace"
    );
    assert_eq!(lines.collect::<Vec<_>>(), ["lo"], "interfaces inside bwrap");

    for (suite, address) in [
        (
            "offline.udp-loopback",
            Some(IpAddr::from(Ipv4Addr::LOCALHOST)),
        ),
        (
            "offline.udp-nonloopback",
            super::nonloopback_addresses().into_iter().next(),
        ),
    ] {
        let Some(address) = address else {
            eprintln!("note: no non-loopback address; {suite} skipped");
            continue;
        };
        let suite = network.suite(suite);
        let Some((socket, late)) = paired_receiver(&network, &suite, address, PROBE_TIMEOUT).await
        else {
            eprintln!(
                "note: {address} does not pair on this host; {} skipped",
                suite.name
            );
            continue;
        };
        assert!(
            network.udp_isolated(socket, &suite).await,
            "bwrap UDP isolation not proven at {address}: {}",
            transcript(&network),
        );
        eprintln!("proof line accepted for {} at {address}", suite.name);
        assert_post_proof_send_stayed_private(&late);
    }
}

/// The proof child sends to the host endpoint after printing its proof. That
/// datagram never joins the host receive queue: a host marker sent afterwards
/// is the first datagram the host reads.
#[cfg(target_os = "linux")]
fn assert_post_proof_send_stayed_private(late: &UdpSocket) {
    let endpoint = late.local_addr().expect("endpoint");
    let marker = b"ego-host-marker";
    UdpSocket::bind((endpoint.ip(), 0))
        .and_then(|sender| sender.send_to(marker, endpoint))
        .expect("host marker sent");
    late.set_nonblocking(false).expect("blocking receiver");
    late.set_read_timeout(Some(PROBE_TIMEOUT))
        .expect("receiver deadline");
    let mut received = [0_u8; 128];
    let (len, _) = late.recv_from(&mut received).expect("host marker");
    assert_eq!(
        &received[..len],
        marker,
        "the post-proof datagram reached the host socket at {endpoint}",
    );
    eprintln!("post-proof send stayed in the private namespace at {endpoint}");
}
