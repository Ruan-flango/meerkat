use std::collections::{HashMap, HashSet};
use crate::runtime::txn::{TxnId, VarState, Transaction};
use tokio::sync::oneshot;
use tokio::time::Duration;
use super::ast::{Value, Decl, Expr, ActionStmt};
use super::interpreter::{eval, EvalContext, EvalError, execute, ExecuteEffect};
use super::semantic_analysis::var_analysis::{calc_dep_srv, DependAnalysis};
use crate::net::{Address, NetworkCommand, NetworkEvent, MeerkatMessage, NetworkActor};
use crate::net::network_layer::NetworkLayer;

pub struct Service {
    pub name: String,
    /// Per-variable state: value, lock, and latest write transaction in one place
    pub vars: HashMap<String, VarState>,
    pub defs: HashMap<String, Expr>,    // original def expressions for re-evaluation
    pub dep: DependAnalysis,            // dependency graph + topo order
}

pub struct Manager {
    pub services: HashMap<String, Service>,
    /// Maps service name to remote address (for distributed services)
    pub remote_services: HashMap<String, Address>,
    /// Network actor for distributed communication
    pub network: Option<NetworkActor>,
    /// Pending reply channels keyed by request_id
    pub pending_replies: HashMap<u64, oneshot::Sender<MeerkatMessage>>,
}

impl Manager {
    pub fn new() -> Self {
        Manager {
            services: HashMap::new(),
            remote_services: HashMap::new(),
            network: None,
            pending_replies: HashMap::new(),
        }
    }

    pub async fn create_service(&mut self, name: String, decls: Vec<Decl>)
        -> Result<(), EvalError>
    {
        let dep = calc_dep_srv(&decls);

        let mut service = Service {
            name: name.clone(),
            vars: HashMap::new(),
            defs: HashMap::new(),
            dep,
        };

        let mut env: Vec<(String, Value)> = vec![];
        let svc_name = name.clone();

        for decl in decls {
            match decl {
                Decl::VarDecl { name, val } => {
                    let value = eval(&val, &env, &mut EvalContext { manager: self, service_name: &svc_name, txn: None }).await?;
                    env.push((name.clone(), value.clone()));
                    service.vars.insert(name, VarState::new(value));
                }
                Decl::DefDecl { name, val, .. } => {
                    let value = eval(&val, &env, &mut EvalContext { manager: self, service_name: &svc_name, txn: None }).await?;
                    env.push((name.clone(), value.clone()));
                    service.vars.insert(name.clone(), VarState::new(value));
                    service.defs.insert(name, val);  // store original expr
                }
                Decl::TableDecl { .. } => {
                    return Err(EvalError::NotImplemented);
                }
            }
        }

        self.services.insert(name.clone(), service);
        Ok(())
    }

    pub async fn lookup(&mut self, ident: &str, service_name: &str, mut txn: Option<&mut Transaction>) -> Result<Value, EvalError> {
        // Check if service is remote
        if self.remote_services.contains_key(service_name) {
            return self.remote_lookup(service_name, ident).await;
        }

        // If it's a def, re-evaluate from stored expression for freshness.
        // The transaction flows through so the def's underlying vars are locked.
        let def_expr = self.services.get(service_name)
            .and_then(|s| s.defs.get(ident))
            .cloned();

        if let Some(expr) = def_expr {
            // Evaluate the def with an empty env so its dependencies resolve
            // through lookup (acquiring read locks and populating the cache)
            // rather than being pre-seeded from current service var values.
            let env: Vec<(String, Value)> = Vec::new();
            return eval(&expr, &env, &mut EvalContext { manager: self, service_name, txn: txn.as_deref_mut() }).await;
        }

        // Local var read. If inside a transaction, return the cached value if
        // present, otherwise acquire a read lock lazily and cache the value.
        // Decide what's needed first (ending the txn borrow) before calling
        // self.acquire_read_lock, then re-borrow txn to record the lock.
        let mut need_read_lock: Option<TxnId> = None;
        if let Some(t) = txn.as_deref() {
            if let Some(cached) = t.read_cache.get(ident) {
                return Ok(cached.clone());
            }
            if !t.locked.contains(ident) {
                need_read_lock = Some(t.id.clone());
            }
        }
        if let Some(txn_id) = need_read_lock {
            self.acquire_read_lock(service_name, ident, &txn_id)?;
            if let Some(t) = txn.as_deref_mut() {
                t.locked.insert(ident.to_string());
            }
        }

        // Return stored var value (and cache it for the transaction)
        if let Some(service) = self.services.get(service_name) {
            if let Some(var_state) = service.vars.get(ident) {
                let value = var_state.value.clone();
                if let Some(t) = txn.as_deref_mut() {
                    t.read_cache.insert(ident.to_string(), value.clone());
                }
                return Ok(value);
            }
        }
        Err(EvalError::LookupError(format!("Variable '{}' not found in service '{}'", ident, service_name)))
    }

    pub async fn assign(&mut self, service_name: &str, var: &str, value: Value, mut txn: Option<&mut Transaction>) -> Result<(), EvalError> {
        // Inside a transaction: acquire the write lock lazily (upgrading from a
        // read lock for read-then-write patterns like x = x + 1) and buffer the
        // write. The buffered value is applied to the service only at commit, so
        // a transaction that fails partway leaves no partial writes behind.
        if txn.is_some() {
            enum LockAction { Acquire, Upgrade }
            let (txn_id, kind) = {
                let t = txn.as_deref().unwrap();
                let kind = if t.locked.contains(var) { LockAction::Upgrade } else { LockAction::Acquire };
                (t.id.clone(), kind)
            };
            match kind {
                LockAction::Upgrade => self.upgrade_to_write_lock(service_name, var, &txn_id)?,
                LockAction::Acquire => self.acquire_write_lock(service_name, var, &txn_id)?,
            }
            if let Some(t) = txn.as_deref_mut() {
                t.locked.insert(var.to_string());
                t.written.insert(var.to_string(), value.clone());
                // Reads later in the same transaction see the buffered write
                t.read_cache.insert(var.to_string(), value);
            }
            return Ok(());
        }

        // Non-transactional path: apply the write immediately and propagate.
        if let Some(service) = self.services.get_mut(service_name) {
            if let Some(var_state) = service.vars.get_mut(var) {
                var_state.value = value;
            } else {
                return Err(EvalError::LookupError(format!("Variable '{}' not found in service '{}'", var, service_name)));
            }
        } else {
            return Err(EvalError::LookupError(format!("Service '{}' not found", service_name)));
        }

        // propagate: re-evaluate defs that depend on this var in topo order
        self.propagate(service_name, var).await
    }

    async fn propagate(&mut self, service_name: &str, changed_var: &str) -> Result<(), EvalError> {
        // collect defs that need re-evaluation in topo order
        let topo_order: Vec<String> = self.services
            .get(service_name)
            .map(|s| s.dep.topo_order.clone())
            .unwrap_or_default();

        for def_name in topo_order {
            let needs_update = self.services
                .get(service_name)
                .and_then(|s| s.dep.dep_vars.get(&def_name))
                .map(|dep_vars| dep_vars.contains(changed_var))
                .unwrap_or(false);

            let is_def = self.services
                .get(service_name)
                .map(|s| s.defs.contains_key(&def_name))
                .unwrap_or(false);

            if needs_update && is_def {
                // build env from current var values
                let expr = self.services
                    .get(service_name)
                    .and_then(|s| s.defs.get(&def_name))
                    .cloned();

                if let Some(expr) = expr {
                    let env: Vec<(String, Value)> = self.services
                        .get(service_name)
                        .map(|s| s.vars.iter().map(|(k, v)| (k.clone(), v.value.clone())).collect())
                        .unwrap_or_default();

                    let value = eval(&expr, &env, &mut EvalContext { manager: self, service_name, txn: None }).await?;

                    if let Some(service) = self.services.get_mut(service_name) {
                        if let Some(var_state) = service.vars.get_mut(&def_name) {
                            var_state.value = value;
                        }
                    }
                }
            }
        }
        Ok(())
    }


    /// Drain all pending network events and dispatch each to the matching
    /// oneshot channel in pending_replies. Non-matching events are dropped.
    pub fn dispatch_network_events(&mut self) {
        loop {
            let event = match self.network.as_mut() {
                Some(n) => n.try_recv_event(),
                None => break,
            };
            match event {
                Some(NetworkEvent::MessageReceived { msg, .. }) => {
                    let rid = match &msg {
                        MeerkatMessage::LookupResponse { request_id, .. } => Some(*request_id),
                        MeerkatMessage::LookupError { request_id, .. } => Some(*request_id),
                        MeerkatMessage::ActionResponse { request_id, .. } => Some(*request_id),
                        _ => None,
                    };
                    if let Some(id) = rid {
                        if let Some(tx) = self.pending_replies.remove(&id) {
                            let _ = tx.send(msg);
                        }
                    }
                }
                Some(_) => {}
                None => break,
            }
        }
    }

    /// Send a message and await a reply using tokio::select! for timeout.
    /// Encapsulates the duplicated send + register channel + await pattern
    /// shared by remote_lookup and remote_action.
    async fn send_and_await_reply(
        &mut self,
        addr: Address,
        msg: MeerkatMessage,
        request_id: u64,
        timeout_msg: String,
    ) -> Result<MeerkatMessage, EvalError> {
        // Send the message
        let net = self.network.as_mut()
            .ok_or_else(|| EvalError::NetworkError("No network layer available".to_string()))?;
        net.handle_command(NetworkCommand::SendMessage { addr, msg }).await;

        // Register oneshot channel for this request
        let (tx, mut rx) = oneshot::channel::<MeerkatMessage>();
        self.pending_replies.insert(request_id, tx);

        // Loop with pinned timeout + tokio::select!. Each iteration dispatches
        // pending network events then checks for reply, timeout, or yields 10ms.
        // The loop is required until the tokio::join! background message loop
        // architecture is implemented as a follow-up.
        let timeout = tokio::time::sleep(Duration::from_secs(15));
        tokio::pin!(timeout);

        loop {
            self.dispatch_network_events();
            tokio::select! {
                biased;
                result = &mut rx => {
                    return result.map_err(|_| {
                        EvalError::NetworkError("Reply channel closed".to_string())
                    });
                }
                _ = &mut timeout => {
                    self.pending_replies.remove(&request_id);
                    return Err(EvalError::NetworkError(timeout_msg));
                }
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
    }

    /// Get the network address for a remote service (strips the slug)
    fn remote_addr(&self, service: &str) -> Result<Address, EvalError> {
        let full_url = self.remote_services.get(service)
            .ok_or_else(|| EvalError::LookupError(format!("Remote service '{}' not found", service)))?;
        let addr_str = full_url.0.trim_end_matches(&format!("/{}", service));
        Ok(Address::new(addr_str))
    }

    /// Get our local address with peer ID for use as reply_to
    /// Replaces loopback/unspecified with the actual outbound IP
    async fn local_reply_addr(&mut self) -> String {
        let net = match self.network.as_mut() {
            Some(n) => n,
            None => return String::new(),
        };
        let peer_id = net.local_peer_id();
        let reply = net.handle_command(NetworkCommand::GetLocalAddresses).await;
        let public_ip = Self::get_public_ip();
        match reply {
            crate::net::NetworkReply::LocalAddresses { addrs } => {
                if let Some(addr) = addrs.first() {
                    let addr_str = addr.0
                        .replace("0.0.0.0", &public_ip)
                        .replace("127.0.0.1", &public_ip);
                    format!("{}/p2p/{}", addr_str, peer_id)
                } else {
                    String::new()
                }
            }
            _ => String::new(),
        }
    }

    /// Get the local machine's outbound IP address (non-loopback)
    pub fn get_public_ip() -> String {
        use std::net::UdpSocket;
        UdpSocket::bind("0.0.0.0:0")
            .and_then(|s| { s.connect("8.8.8.8:80")?; s.local_addr() })
            .map(|addr| addr.ip().to_string())
            .unwrap_or_else(|_| "127.0.0.1".to_string())
    }

    pub async fn remote_lookup(&mut self, service: &str, member: &str) -> Result<Value, EvalError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);

        let addr = self.remote_addr(service)?;
        let request_id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let reply_to = self.local_reply_addr().await;

        let msg = MeerkatMessage::LookupRequest {
            request_id,
            service: service.to_string(),
            member: member.to_string(),
            reply_to,
        };

        let reply = self.send_and_await_reply(
            addr, msg, request_id,
            format!("Timeout waiting for remote lookup of {}.{}", service, member),
        ).await?;

        match reply {
            MeerkatMessage::LookupResponse { value, .. } => {
                let val: Value = serde_json::from_str(&value)
                    .map_err(|e| EvalError::NetworkError(e.to_string()))?;
                Ok(val)
            }
            MeerkatMessage::LookupError { error, .. } => {
                Err(EvalError::LookupError(error))
            }
            _ => Err(EvalError::NetworkError("Unexpected reply to lookup request".to_string())),
        }
    }

    pub async fn remote_action(&mut self, service: &str, stmts: Vec<ActionStmt>, env: Vec<(String, Value)>) -> Result<(), EvalError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ACTION_ID: AtomicU64 = AtomicU64::new(1);

        let addr = self.remote_addr(service)?;
        let request_id = NEXT_ACTION_ID.fetch_add(1, Ordering::SeqCst);
        let reply_to = self.local_reply_addr().await;

        let msg = MeerkatMessage::ActionRequest {
            request_id,
            service: service.to_string(),
            stmts,
            env,
            reply_to,
        };

        let reply = self.send_and_await_reply(
            addr, msg, request_id,
            format!("Timeout waiting for remote action on service '{}'", service),
        ).await?;

        match reply {
            MeerkatMessage::ActionResponse { success, error, .. } => {
                if success {
                    Ok(())
                } else {
                    Err(EvalError::NetworkError(
                        error.unwrap_or_else(|| "Remote action failed".to_string())
                    ))
                }
            }
            _ => Err(EvalError::NetworkError("Unexpected reply to action request".to_string())),
        }
    }

    /// Try to acquire a write lock on a service variable.
    /// Returns LockConflict if the variable is already locked.
    fn acquire_write_lock(&mut self, service_name: &str, var: &str, txn_id: &TxnId) -> Result<(), EvalError> {
        let service = self.services.get_mut(service_name)
            .ok_or_else(|| EvalError::LookupError(format!("Service '{}' not found", service_name)))?;
        let var_state = service.vars.get_mut(var)
            .ok_or_else(|| EvalError::LookupError(format!("Variable '{}' not found", var)))?;
        if var_state.lock.try_write(txn_id) {
            Ok(())
        } else {
            Err(EvalError::LockConflict(format!(
                "Variable '{}' is already locked; cannot acquire write lock", var
            )))
        }
    }

    /// Try to acquire a read lock on a service variable.
    /// Returns LockConflict if a write lock is held.
    fn acquire_read_lock(&mut self, service_name: &str, var: &str, txn_id: &TxnId) -> Result<(), EvalError> {
        let service = self.services.get_mut(service_name)
            .ok_or_else(|| EvalError::LookupError(format!("Service '{}' not found", service_name)))?;
        let var_state = service.vars.get_mut(var)
            .ok_or_else(|| EvalError::LookupError(format!("Variable '{}' not found", var)))?;
        if var_state.lock.try_read(txn_id) {
            Ok(())
        } else {
            Err(EvalError::LockConflict(format!(
                "Variable '{}' is write-locked by another transaction", var
            )))
        }
    }

    /// Upgrade a read lock to a write lock on a service variable.
    /// Used for read-then-write within the same transaction (e.g. x = x + 1).
    fn upgrade_to_write_lock(&mut self, service_name: &str, var: &str, txn_id: &TxnId) -> Result<(), EvalError> {
        let service = self.services.get_mut(service_name)
            .ok_or_else(|| EvalError::LookupError(format!("Service '{}' not found", service_name)))?;
        let var_state = service.vars.get_mut(var)
            .ok_or_else(|| EvalError::LookupError(format!("Variable '{}' not found", var)))?;
        if var_state.lock.upgrade_to_write(txn_id) {
            Ok(())
        } else {
            Err(EvalError::LockConflict(format!(
                "Variable '{}' cannot be upgraded to a write lock; held by another transaction", var
            )))
        }
    }

    /// Release all locks held by txn_id on the given variables.
    fn release_locks(&mut self, service_name: &str, vars: &HashSet<String>, txn_id: &TxnId) {
        if let Some(service) = self.services.get_mut(service_name) {
            for var in vars {
                if let Some(var_state) = service.vars.get_mut(var) {
                    var_state.lock.release(txn_id);
                }
            }
        }
    }

    /// Execute action statements as a transaction with lazy lock acquisition:
    ///
    /// Locks are acquired on demand as each variable is first read or written
    /// during execution (inside `lookup` and `assign`), rather than upfront.
    /// This handles actions invoked via function calls and conditional branches,
    /// where the set of accessed variables can't be determined statically.
    /// Read values are cached in the transaction to avoid re-fetching (which
    /// also avoids redundant network round-trips for remote reads).
    ///
    /// On completion: commit records latest_write_txn for written variables,
    /// then all locks are released (always, even on error).
    ///
    /// Deadlock prevention (wait-die) is deferred to a follow-up issue.
    /// If a lock cannot be acquired, the transaction fails immediately.
    pub async fn execute_action_with_txn(
        &mut self,
        service_name: &str,
        stmts: &[ActionStmt],
        initial_env: &[(String, Value)],
    ) -> Result<(), EvalError> {
        // The transaction owns all its state and is passed down through
        // execution; nothing transaction-specific lives on the Manager.
        let mut txn = Transaction::new(TxnId::new());

        // Execute statements; read/write locks are acquired lazily inside
        // lookup/assign as variables are accessed
        let mut env: Vec<(String, Value)> = initial_env.to_vec();
        let mut exec_error: Option<EvalError> = None;
        for stmt in stmts {
            match execute(stmt, &env, self, service_name, Some(&mut txn)).await {
                Ok(ExecuteEffect::Binding(name, val)) => env.push((name, val)),
                Ok(_) => {}
                Err(e) => { exec_error = Some(e); break; }
            }
        }

        // Commit — apply the buffered writes to the service, record the writing
        // transaction, then propagate to dependent defs. On execution error
        // nothing is applied, so the transaction leaves no partial writes.
        let mut commit_error: Option<EvalError> = None;
        if exec_error.is_none() {
            let writes: Vec<(String, Value)> = txn.written.iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            for (var, value) in &writes {
                if let Some(service) = self.services.get_mut(service_name) {
                    if let Some(var_state) = service.vars.get_mut(var) {
                        var_state.value = value.clone();
                        var_state.latest_write_txn = Some(txn.id.clone());
                    }
                }
            }
            // Propagate after all writes are applied so defs see a consistent state
            for (var, _) in &writes {
                if let Err(e) = self.propagate(service_name, var).await {
                    commit_error = Some(e);
                    break;
                }
            }
        }

        // Release all locks (always, even on error)
        self.release_locks(service_name, &txn.locked, &txn.id);

        match exec_error.or(commit_error) {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    pub async fn execute_action(&mut self, service_name: &str, stmts: &[ActionStmt]) -> Result<(), EvalError> {
        self.execute_action_with_txn(service_name, stmts, &[]).await
    }

    pub async fn execute_action_with_env(&mut self, service_name: &str, stmts: &[ActionStmt], initial_env: &[(String, Value)]) -> Result<(), EvalError> {
        self.execute_action_with_txn(service_name, stmts, initial_env).await
    }
}

impl Default for Manager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Decl, Expr, Value};

    #[tokio::test]
    async fn test_create_service_with_var() {
        let mut manager = Manager::new();
        let decls = vec![
            Decl::VarDecl {
                name: "x".to_string(),
                val: Expr::Literal { val: Value::Number { val: 1 } },
            },
        ];
        manager.create_service("foo".to_string(), decls).await.unwrap();
        let result = manager.lookup("x", "foo", None).await.unwrap();
        assert_eq!(result, Value::Number { val: 1 });
    }

    #[tokio::test]
    async fn test_create_service_with_def() {
        let mut manager = Manager::new();
        let decls = vec![
            Decl::VarDecl {
                name: "x".to_string(),
                val: Expr::Literal { val: Value::Number { val: 2 } },
            },
            Decl::DefDecl {
                name: "f".to_string(),
                val: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Variable { ident: "x".to_string() }),
                    expr2: Box::new(Expr::Literal { val: Value::Number { val: 3 } }),
                },
                is_pub: true,
            },
        ];
        manager.create_service("foo".to_string(), decls).await.unwrap();
        let result = manager.lookup("f", "foo", None).await.unwrap();
        assert_eq!(result, Value::Number { val: 5 });
    }

    #[tokio::test]
    async fn test_lookup_missing_var_returns_error() {
        let mut manager = Manager::new();
        manager.create_service("foo".to_string(), vec![]).await.unwrap();
        let result = manager.lookup("nonexistent", "foo", None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_def_updates_after_var_change() {
        let mut manager = Manager::new();
        // service foo { var x = 1; def f = x + 10; }
        let decls = vec![
            Decl::VarDecl {
                name: "x".to_string(),
                val: Expr::Literal { val: Value::Number { val: 1 } },
            },
            Decl::DefDecl {
                name: "f".to_string(),
                val: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Variable { ident: "x".to_string() }),
                    expr2: Box::new(Expr::Literal { val: Value::Number { val: 10 } }),
                },
                is_pub: true,
            },
        ];
        manager.create_service("foo".to_string(), decls).await.unwrap();

        // f should be 11 initially
        let result = manager.lookup("f", "foo", None).await.unwrap();
        assert_eq!(result, Value::Number { val: 11 });

        // update x to 5, f should become 15
        manager.assign("foo", "x", Value::Number { val: 5 }, None).await.unwrap();
        let result = manager.lookup("f", "foo", None).await.unwrap();
        assert_eq!(result, Value::Number { val: 15 });
    }

    // Helper: service with a single var x = 0
    async fn manager_with_x() -> Manager {
        let mut manager = Manager::new();
        let decls = vec![
            Decl::VarDecl {
                name: "x".to_string(),
                val: Expr::Literal { val: Value::Number { val: 0 } },
            },
        ];
        manager.create_service("foo".to_string(), decls).await.unwrap();
        manager
    }

    fn x_state(manager: &Manager) -> &VarState {
        manager.services.get("foo").unwrap().vars.get("x").unwrap()
    }

    fn assert_x_unlocked(manager: &Manager) {
        assert!(matches!(
            &x_state(manager).lock,
            crate::runtime::txn::VarLock::Unlocked
        ));
    }

    // x = x + 1 reads x (read lock) then writes x (must upgrade to write lock).
    // This is the read-then-write pattern that the old upfront analysis mishandled.
    #[tokio::test]
    async fn test_txn_read_then_write_upgrades_lock() {
        let mut manager = manager_with_x().await;
        let stmts = vec![
            ActionStmt::Assign {
                var: "x".to_string(),
                expr: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Variable { ident: "x".to_string() }),
                    expr2: Box::new(Expr::Literal { val: Value::Number { val: 1 } }),
                },
            },
        ];
        manager.execute_action("foo", &stmts).await.unwrap();
        let result = manager.lookup("x", "foo", None).await.unwrap();
        assert_eq!(result, Value::Number { val: 1 });
    }

    // Locks must be released after a transaction, so a second transaction
    // can acquire them. Running x = x + 1 twice should yield x == 2.
    #[tokio::test]
    async fn test_txn_locks_released_between_transactions() {
        let mut manager = manager_with_x().await;
        let stmts = vec![
            ActionStmt::Assign {
                var: "x".to_string(),
                expr: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Variable { ident: "x".to_string() }),
                    expr2: Box::new(Expr::Literal { val: Value::Number { val: 1 } }),
                },
            },
        ];
        manager.execute_action("foo", &stmts).await.unwrap();
        manager.execute_action("foo", &stmts).await.unwrap();
        let result = manager.lookup("x", "foo", None).await.unwrap();
        assert_eq!(result, Value::Number { val: 2 });
    }

    // After a transaction completes, the variable's lock should be Unlocked.
    #[tokio::test]
    async fn test_txn_var_unlocked_after_commit() {
        let mut manager = manager_with_x().await;
        let stmts = vec![
            ActionStmt::Assign {
                var: "x".to_string(),
                expr: Expr::Literal { val: Value::Number { val: 42 } },
            },
        ];
        manager.execute_action("foo", &stmts).await.unwrap();
        assert_x_unlocked(&manager);
    }

    // A successful transaction commits its buffered write and records the
    // transaction as the latest writer for that variable.
    #[tokio::test]
    async fn test_txn_successful_write_updates_value_and_latest_write_txn() {
        let mut manager = manager_with_x().await;
        let stmts = vec![
            ActionStmt::Assign {
                var: "x".to_string(),
                expr: Expr::Literal { val: Value::Number { val: 42 } },
            },
        ];

        manager.execute_action("foo", &stmts).await.unwrap();

        let state = x_state(&manager);
        assert_eq!(state.value, Value::Number { val: 42 });
        assert!(state.latest_write_txn.is_some());
    }

    // A nested `do` (an action invoking another action) must reuse the same
    // transaction, not start a fresh one. The inner write to x should commit and
    // all locks should be released afterward. This guards the bug where nested
    // execution clobbered the outer transaction's lock tracking.
    #[tokio::test]
    async fn test_txn_nested_do_reuses_transaction() {
        let mut manager = manager_with_x().await;
        // outer action: do (action { x = x + 1; });
        let inner = Expr::Action(vec![
            ActionStmt::Assign {
                var: "x".to_string(),
                expr: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Variable { ident: "x".to_string() }),
                    expr2: Box::new(Expr::Literal { val: Value::Number { val: 1 } }),
                },
            },
        ]);
        let stmts = vec![ActionStmt::Do(inner)];
        manager.execute_action("foo", &stmts).await.unwrap();

        // inner write took effect
        let result = manager.lookup("x", "foo", None).await.unwrap();
        assert_eq!(result, Value::Number { val: 1 });
        // and the lock was released
        assert_x_unlocked(&manager);
    }

    // A transaction that fails partway must leave no partial writes: writes are
    // buffered and applied only on a successful commit. Here the first statement
    // writes x, the second fails (asserting false), so x must stay unchanged.
    #[tokio::test]
    async fn test_txn_failed_transaction_leaves_no_partial_writes() {
        let mut manager = manager_with_x().await;
        let stmts = vec![
            ActionStmt::Assign {
                var: "x".to_string(),
                expr: Expr::Literal { val: Value::Number { val: 99 } },
            },
            ActionStmt::Assert(Expr::Literal { val: Value::Bool { val: false } }),
        ];
        let result = manager.execute_action("foo", &stmts).await;
        assert!(result.is_err());
        // x must remain 0 — the buffered write to 99 was never committed
        let x = manager.lookup("x", "foo", None).await.unwrap();
        assert_eq!(x, Value::Number { val: 0 });
        // and the lock was released
        assert_x_unlocked(&manager);
    }

    // A failed transaction must not update either committed state field: the
    // value and latest writer should remain from the last successful commit.
    #[tokio::test]
    async fn test_txn_failed_transaction_preserves_previous_latest_write_txn() {
        let mut manager = manager_with_x().await;
        let successful_write = vec![
            ActionStmt::Assign {
                var: "x".to_string(),
                expr: Expr::Literal { val: Value::Number { val: 1 } },
            },
        ];
        manager.execute_action("foo", &successful_write).await.unwrap();
        let previous_txn = x_state(&manager).latest_write_txn.clone();
        assert!(previous_txn.is_some());

        let failing_write = vec![
            ActionStmt::Assign {
                var: "x".to_string(),
                expr: Expr::Literal { val: Value::Number { val: 99 } },
            },
            ActionStmt::Assert(Expr::Literal { val: Value::Bool { val: false } }),
        ];

        let result = manager.execute_action("foo", &failing_write).await;

        assert!(result.is_err());
        let state = x_state(&manager);
        assert_eq!(state.value, Value::Number { val: 1 });
        assert_eq!(state.latest_write_txn, previous_txn);
        assert_x_unlocked(&manager);
    }

    // If a transaction fails after a read, its read lock must still be released.
    #[tokio::test]
    async fn test_txn_read_lock_released_after_failure() {
        let mut manager = manager_with_x().await;
        let stmts = vec![
            ActionStmt::Assert(Expr::Variable { ident: "x".to_string() }),
        ];

        let result = manager.execute_action("foo", &stmts).await;

        assert!(result.is_err());
        assert_eq!(x_state(&manager).value, Value::Number { val: 0 });
        assert!(x_state(&manager).latest_write_txn.is_none());
        assert_x_unlocked(&manager);
    }
}
