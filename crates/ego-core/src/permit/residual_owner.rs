//! Owner grants cross the same append-before-publication boundary as judge decisions.
use super::*;

pub struct OwnerGrantRequest {
    pub grant: SandboxGrant,
    reply: oneshot::Sender<()>,
}

impl ResidualWriteDesk {
    pub fn limits(&self) -> ResidualLimits {
        *self.limits.lock().expect("residual limits")
    }

    pub fn configure_limits(&self, limits: ResidualLimits) {
        *self.limits.lock().expect("residual limits") = limits;
    }

    /// Called from the executing session's replayed state, never an ancestor.
    pub fn restore_owner_grants(&self, grants: &[SandboxGrant]) {
        let mut ledger = self.ledger();
        for grant in grants {
            if !ledger.owner.contains(grant) {
                ledger.owner.push(grant.clone());
            }
        }
    }

    pub fn owner_grants(&self) -> Vec<SandboxGrant> {
        self.ledger().owner.clone()
    }

    /// The host already approved this exact request. Wait for its durable receipt.
    pub async fn record_owner_grant(&self, grant: SandboxGrant) -> bool {
        let (reply, answer) = oneshot::channel();
        if self
            .owner_sender
            .unbounded_send(OwnerGrantRequest { grant, reply })
            .is_err()
        {
            return false;
        }
        answer.await.is_ok()
    }

    pub async fn next_owner(&self) -> Option<OwnerGrantRequest> {
        self.owner_receiver.lock().await.next().await
    }

    pub fn answer_owner(&self, request: OwnerGrantRequest) {
        self.restore_owner_grants(std::slice::from_ref(&request.grant));
        let _ = request.reply.send(());
    }
}
