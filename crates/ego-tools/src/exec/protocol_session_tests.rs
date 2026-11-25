#![cfg(unix)]

use std::time::Duration;

use super::*;

// Catches: a command whose send permit was taken before the owner stopped is
// written after the receiver drained, so its reply never closes and exchange waits forever.
#[tokio::test]
async fn command_admitted_before_owner_stops_still_has_its_reply_closed() {
    let request = SecretChildRequest {
        argv: vec!["/bin/sh".into(), "-c".into(), "exec /bin/cat".into()],
        cwd: None,
        stdin: None,
        cancel: CancelToken::new(),
        run_deadline: None,
        command_timeout: Duration::from_secs(2),
        termination_grace: Duration::from_millis(100),
    };
    let cancel = request.cancel.clone();
    let mut child = start_protocol_session(request, 2).expect("start");
    // The first half of `Sender::send`: the permit is taken while the owner is idle.
    let permit = child
        .commands
        .clone()
        .reserve_owned()
        .await
        .expect("permit");
    cancel.cancel(CancelCause::Host);
    // The owner publishes its terminal outcome only after it left own_session.
    assert!(matches!(
        (&mut child.terminal).await,
        Ok(Err(SecretChildError::Cancelled))
    ));
    // The second half of `Sender::send`: the command lands after the owner stopped.
    let (reply, receiver) = oneshot::channel();
    permit.send(Command::Exchange(SecretMaterial::from("{}"), reply));
    assert!(
        matches!(
            tokio::time::timeout(Duration::from_secs(5), receiver).await,
            Ok(Err(_))
        ),
        "the owner must close the reply of every admitted command"
    );
}
