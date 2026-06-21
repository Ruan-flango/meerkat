use super::ast::{ActionStmt, Decl, Expr, Value};
use super::interpreter::{eval, execute, EvalContext, EvalError, ExecuteEffect};
use super::semantic_analysis::var_analysis::{calc_dep_srv, DependAnalysis};
use crate::net::network_layer::NetworkLayer;
use crate::net::{
    codec, Address, MeerkatMessage, NetworkActor, NetworkCommand, NetworkEvent, NetworkReply,
    ServiceNetId,
};
use crate::runtime::interner::{Interner, Symbol};
use crate::runtime::txn::{Transaction, TxnId, VarState};
use std::collections::{HashMap, HashSet};
use tokio::sync::oneshot;
use tokio::time::Duration;

pub struct Service {
    /// Globally unique identity of this service (address-based when networked).
    pub id: ServiceNetId,
    pub name: Symbol,
    /// Per-variable state: value, lock, and latest write transaction in one place
    pub vars: HashMap<Symbol, VarState>,
    pub defs: HashMap<Symbol, Expr>, // original def expressions for re-evaluation
    pub dep: DependAnalysis,         // dependency graph + topo order
    /// #24: who depends on each member: member -> {(listener service id, def)}.
    pub listeners: HashMap<Symbol, HashSet<(ServiceNetId, Symbol)>>,
    /// #24: cached values of each def's cross-service deps:
    /// def -> {(source service, member) -> value}.
    pub dep_cache: HashMap<Symbol, HashMap<(Symbol, Symbol), Value>>,
    /// #24: each def's direct cross-service deps as (service, member) symbols.
    pub dep_remote: HashMap<Symbol, HashSet<(Symbol, Symbol)>>,
}

/// A remote request parked on a variable's wait queue because the requesting
/// transaction is older than the current lock holder (wait-die wait). It holds
/// everything needed to re-dispatch the request and send its deferred reply
/// once the contended lock frees.
pub enum ParkedRequest {
    Action {
        request_id: u64,
        reply_to: String,
        service: Symbol,
        stmts: Vec<ActionStmt>,
        env: Vec<(Symbol, Value)>,
        tid: TxnId,
    },
    Lookup {
        request_id: u64,
        reply_to: String,
        service: Symbol,
        member: Symbol,
        tid: TxnId,
    },
}

impl ParkedRequest {
    /// The transaction this parked request belongs to. Its age decides serve
    /// order, and identifies it for purging when that transaction aborts.
    pub fn tid(&self) -> &TxnId {
        match self {
            ParkedRequest::Action { tid, .. } => tid,
            ParkedRequest::Lookup { tid, .. } => tid,
        }
    }
}

pub struct Manager {
    pub services: HashMap<Symbol, Service>,
    /// Maps service name to remote address (for distributed services)
    pub remote_services: HashMap<Symbol, Address>,
    /// Network actor for distributed communication
    pub network: Option<NetworkActor>,
    /// Pending reply channels keyed by request_id
    pub pending_replies: HashMap<u64, oneshot::Sender<MeerkatMessage>>,
    /// (Probabilistically) unique identifier of this node, used in transaction
    /// ids so ids minted on different nodes never collide.
    pub node_id: u64,
    /// Distributed transactions this node is participating in: actions composed
    /// by a remote originator, executed under a shared id and held (locks +
    /// buffered writes) until a Commit or Abort arrives.
    pub pending_txns: HashMap<TxnId, Transaction>,
    /// Requests parked because the requesting transaction is older than a lock
    /// holder (wait-die wait), keyed by the contended (service, var). Drained
    /// oldest-first when that variable's lock frees on commit or abort.
    pub wait_queue: HashMap<(ServiceNetId, Symbol), Vec<ParkedRequest>>,
    /// This node's canonical, dialable address, set once after the network is
    /// listening. Service identities are derived from it, so they are stable for
    /// the life of the process (never empty-then-populated) and match the URL
    /// under which the node advertises its services.
    local_address: Option<String>,
    /// Enable local loopback mode
    pub local: bool,
    /// String interner
    pub interner: Interner,
    /// #24: transient cache consulted during a reactive recompute. Holds the
    /// (service, member) -> value map for the def currently being recomputed so
    /// MemberAccess resolves from cache instead of a (possibly remote) lookup.
    pub reactive_cache: Option<HashMap<(Symbol, Symbol), Value>>,
    /// #24: reply address for each remote listener, keyed by the listener's
    /// ServiceNetId, so the owner can route Updates back to it.
    pub listener_addrs: HashMap<ServiceNetId, String>,
}

impl Manager {
    pub fn new(interner: Interner) -> Self {
        Manager {
            services: HashMap::new(),
            remote_services: HashMap::new(),
            network: None,
            pending_replies: HashMap::new(),
            node_id: Self::random_node_id(),
            pending_txns: HashMap::new(),
            wait_queue: HashMap::new(),
            local_address: None,
            local: false,
            interner,
            reactive_cache: None,
            listener_addrs: HashMap::new(),
        }
    }

    /// Park a request on the wait queue for the contended `(service, var)`
    /// It receives no reply until that variable's lock frees and it is
    /// re-dispatched
    pub fn park_request(&mut self, service: Symbol, var: Symbol, parked: ParkedRequest) {
        let key = (self.service_net_id_for_name(service), var);
        self.wait_queue.entry(key).or_default().push(parked);
    }

    /// After a holder releases locks on commit or abort, return the oldest
    /// parked request waiting on each freed (service, var), removing it from the
    /// queue. Serving the oldest first is what keeps an older transaction from
    /// being starved by a stream of younger requests.
    pub fn take_ready_waiters(
        &mut self,
        freed: &HashSet<(ServiceNetId, Symbol)>,
    ) -> Vec<ParkedRequest> {
        let mut ready = Vec::new();
        for key in freed {
            if let Some(waiters) = self.wait_queue.get_mut(key) {
                if let Some(idx) = waiters
                    .iter()
                    .enumerate()
                    .min_by(|(_, a), (_, b)| a.tid().cmp(b.tid()))
                    .map(|(i, _)| i)
                {
                    ready.push(waiters.remove(idx));
                    if waiters.is_empty() {
                        self.wait_queue.remove(key);
                    }
                }
            }
        }
        ready
    }

    /// Remove and return all parked requests belonging to a transaction, used
    /// when it aborts so its waiters do not later wake and prepare locks for a
    /// transaction the originator has abandoned.
    pub fn purge_parked_txn(&mut self, tid: &TxnId) -> Vec<ParkedRequest> {
        let mut removed = Vec::new();
        for waiters in self.wait_queue.values_mut() {
            let mut i = 0;
            while i < waiters.len() {
                if waiters[i].tid() == tid {
                    removed.push(waiters.remove(i));
                } else {
                    i += 1;
                }
            }
        }
        self.wait_queue.retain(|_, v| !v.is_empty());
        removed
    }

    /// (request_id, reply_to) for every currently parked request, so the owner
    /// can periodically reassure waiting originators that they are still queued
    /// (keepalive), keeping the wait from hitting the reply timeout.
    pub fn parked_keepalive_targets(&self) -> Vec<(u64, String)> {
        let mut out = Vec::new();
        for waiters in self.wait_queue.values() {
            for p in waiters {
                let pair = match p {
                    ParkedRequest::Action {
                        request_id,
                        reply_to,
                        ..
                    } => (*request_id, reply_to.clone()),
                    ParkedRequest::Lookup {
                        request_id,
                        reply_to,
                        ..
                    } => (*request_id, reply_to.clone()),
                };
                out.push(pair);
            }
        }
        out
    }

    /// Record this node's canonical address once the network is listening,
    /// so service identities are stable and consistent with the
    /// advertised URL
    pub fn set_local_address(&mut self, addr: String) {
        self.local_address = Some(addr);
    }

    /// Compute the global identity of a service owned by this node. When
    /// the node has a network address, the identity is that address plus
    /// the service slug; otherwise it falls back to the bare name for
    /// local-only execution
    fn compute_service_net_id(&self, service_name: Symbol) -> ServiceNetId {
        let name_str = self.interner.get(service_name);
        match &self.local_address {
            Some(addr) if !addr.is_empty() => ServiceNetId::new(format!("{}/{}", addr, name_str)),
            Some(_) | None => {
                // No network address: fall back to the bare name. On a
                // single node names are unambiguous, and because
                // `local_address` is fixed at startup this choice never
                // changes mid-run
                ServiceNetId::new(name_str)
            }
        }
    }

    pub async fn create_service(
        &mut self,
        name: Symbol,
        decls: Vec<Decl>,
    ) -> Result<(), EvalError> {
        let dep = calc_dep_srv(&decls);

        // #24: extract each def's direct cross-service deps now, while we still
        // own `decls`. free_var drops MemberAccess, so this is the only place a
        // reference like `s1.y` becomes visible to the runtime.
        let mut dep_remote: HashMap<Symbol, HashSet<(Symbol, Symbol)>> = HashMap::new();
        for decl in &decls {
            if let Decl::DefDecl { name, val, .. } = decl {
                let refs = val.cross_service_deps();
                if !refs.is_empty() {
                    dep_remote.insert(*name, refs);
                }
            }
        }

        let id = self.compute_service_net_id(name);
        // Register the service (with its real `ServiceNetId`) before
        // evaluating any declarations, so action closures built during
        // initialization are stamped with the correct `ServiceNetId`
        // instead of `service_net_id_for_name`'s bare-name fallback
        self.services.insert(
            name,
            Service {
                id,
                name,
                vars: HashMap::new(),
                defs: HashMap::new(),
                dep,
                listeners: HashMap::new(),
                dep_cache: HashMap::new(),
                dep_remote,
            },
        );

        let mut env: Vec<(Symbol, Value)> = vec![];
        let svc_name = name;

        let mut txn = Transaction::new(TxnId::new(self.node_id));
        let mut init_error = None;

        for decl in decls {
            match decl {
                Decl::VarDecl { name, ty: _, val } => {
                    let value = match eval(
                        &val,
                        &env,
                        &mut EvalContext {
                            manager: self,
                            service_name: svc_name,
                            txn: Some(&mut txn),
                        },
                    )
                    .await
                    {
                        Ok(v) => v,
                        Err(e) => {
                            init_error = Some(e);
                            break;
                        }
                    };
                    env.push((name, value.clone()));
                    if let Some(service) = self.services.get_mut(&svc_name) {
                        let mut var_value = VarState::new(value);
                        var_value.latest_write_txn = Some(txn.id.clone());
                        service.vars.insert(name, var_value);
                    }
                }
                Decl::DefDecl { name, val, .. } => {
                    let value = match eval(
                        &val,
                        &env,
                        &mut EvalContext {
                            manager: self,
                            service_name: svc_name,
                            txn: Some(&mut txn),
                        },
                    )
                    .await
                    {
                        Ok(v) => v,
                        Err(e) => {
                            init_error = Some(e);
                            break;
                        }
                    };
                    env.push((name, value.clone()));
                    if let Some(service) = self.services.get_mut(&svc_name) {
                        let mut var_value = VarState::new(value);
                        var_value.latest_write_txn = Some(txn.id.clone());
                        service.vars.insert(name, var_value);
                        service.defs.insert(name, val); // store original expr
                    }
                }
                Decl::TableDecl { .. } => {
                    // we still need to release locks, so no longer return directly after
                    // encountering a TableDecl
                    init_error = Some(EvalError::NotImplemented);
                    break;
                }
            }
        }

        // #87: commit on success, abort and roll back on failure (pattern from
        // execute_action_with_txn).
        if init_error.is_none() {
            self.apply_committed_writes(&txn).await;
            for addr in txn.participants.iter().cloned().collect::<Vec<_>>() {
                let _ = self.send_commit(addr, &txn.id).await;
            }

            // #24: now that init succeeded, register listener edges so a change to
            // a member notifies the defs that depend on it. Same-service deps come
            // from dep_graph; cross-service deps come from dep_remote, where a local
            // owner is wired in-process and a remote owner is subscribed over the
            // wire. Only runs on the success path: a rolled-back service must not
            // register listeners.
            if let Some(this_id) = self.services.get(&svc_name).map(|s| s.id.clone()) {
                let mut edges: Vec<(Symbol, Symbol, Symbol)> = Vec::new();
                if let Some(s) = self.services.get(&svc_name) {
                    for (def_name, deps) in &s.dep.dep_graph {
                        if s.defs.contains_key(def_name) {
                            for dep_member in deps {
                                edges.push((svc_name, *dep_member, *def_name));
                            }
                        }
                    }
                    for (def_name, refs) in &s.dep_remote {
                        for (owner, member) in refs {
                            edges.push((*owner, *member, *def_name));
                        }
                    }
                }
                for (owner, member, listener_def) in edges {
                    if self.services.contains_key(&owner) {
                        if let Some(owner_svc) = self.services.get_mut(&owner) {
                            owner_svc
                                .listeners
                                .entry(member)
                                .or_default()
                                .insert((this_id.clone(), listener_def));
                        }
                    } else {
                        // remote owner: subscribe over the wire so future changes push.
                        self.subscribe_remote(owner, member, this_id.clone(), listener_def)
                            .await;
                    }
                }
            }
        } else {
            // Execution failed: discard buffered writes and abort participants.
            for addr in txn.participants.iter().cloned().collect::<Vec<_>>() {
                self.send_abort(addr, &txn.id).await;
            }
            self.services.remove(&svc_name);
        }

        // Release all locks held locally (always, even on error)
        self.release_locks(&txn.locked, &txn.id);

        match init_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Lookup a variable or def's value within a service
    ///
    /// Evaluates stored def expressions to ensure freshness and acquires
    /// appropriate read locks when executed inside a transaction.
    ///
    /// Args:
    ///     var_name (Symbol): The symbol of the variable or definition to look up
    ///     service_name (Symbol): The symbol of the service containing the variable
    ///     txn (Option<&mut Transaction>): An optional active transaction context
    ///
    /// Returns:
    ///     Result<Value, EvalError>: The retrieved runtime value, or an error
    ///
    /// Raises:
    ///     EvalError::VarNotFound: If the variable does not exist in the service
    ///     EvalError::ServiceNotFound: If the service is not found
    pub async fn lookup(
        &mut self,
        var_name: Symbol,
        service_name: Symbol,
        mut txn: Option<&mut Transaction>,
    ) -> Result<Value, EvalError> {
        // Check if service is remote
        if self.remote_services.contains_key(&service_name) {
            return self.remote_lookup(service_name, var_name, txn).await;
        }

        // If it's a def, re-evaluate from stored expression for freshness.
        // The transaction flows through so the def's underlying vars are locked.
        let def_expr = self
            .services
            .get(&service_name)
            .and_then(|s| s.defs.get(&var_name))
            .cloned();

        if let Some(expr) = def_expr {
            // Evaluate the def with an empty env so its dependencies resolve
            // through lookup (acquiring read locks and populating the cache)
            // rather than being pre-seeded from current service var values.
            let env: Vec<(Symbol, Value)> = Vec::new();
            return eval(
                &expr,
                &env,
                &mut EvalContext {
                    manager: self,
                    service_name,
                    txn: txn.as_deref_mut(),
                },
            )
            .await;
        }

        // Local var read. If inside a transaction, return the cached value if
        // present, otherwise acquire a read lock lazily and cache the value.
        // Transaction state is keyed by (service id, variable) so the same name
        // in different services never collides.
        let key = (self.service_net_id_for_name(service_name), var_name);
        let mut need_read_lock: Option<TxnId> = None;
        if let Some(t) = txn.as_deref() {
            if let Some(cached) = t.read_cache.get(&key) {
                return Ok(cached.clone());
            }
            if !t.locked.contains(&key) {
                need_read_lock = Some(t.id.clone());
            }
        }
        if let Some(txn_id) = need_read_lock {
            self.acquire_read_lock(service_name, var_name, &txn_id)?;
            if let Some(t) = txn.as_deref_mut() {
                t.locked.insert(key.clone());
            }
        }

        // Return stored var value (and cache it for the transaction)
        if let Some(service) = self.services.get(&service_name) {
            if let Some(var_state) = service.vars.get(&var_name) {
                let value = var_state.value.clone();
                if let Some(t) = txn {
                    t.read_cache.insert(key, value.clone());
                }
                return Ok(value);
            }
        }
        Err(EvalError::VarNotFound(format!(
            "Variable '{}' not found in service '{}'",
            self.interner.get(var_name),
            self.interner.get(service_name)
        )))
    }

    /// Assign a value to a service variable
    ///
    /// In a transaction, this acquires a write lock (or upgrades an existing read lock)
    /// and buffers the write. Non-transactional writes are applied immediately and propagated.
    ///
    /// Args:
    ///     service_name (Symbol): The symbol of the service containing the variable
    ///     var_name (Symbol): The symbol of the variable to assign
    ///     value (Value): The value to assign to the variable
    ///     txn (Option<&mut Transaction>): An optional active transaction context
    ///
    /// Returns:
    ///     Result<(), EvalError>: Ok on success, or a lock/validation error
    ///
    /// Raises:
    ///     EvalError::VarNotFound: If the variable does not exist in the service
    ///     EvalError::ServiceNotFound: If the service is not found
    ///     EvalError::WaitDieAbort: If the transaction aborts due to lock contention
    pub async fn assign(
        &mut self,
        service_name: Symbol,
        var_name: Symbol,
        value: Value,
        txn: Option<&mut Transaction>,
    ) -> Result<(), EvalError> {
        // Inside a transaction: acquire the write lock lazily (upgrading from a
        // read lock for read-then-write patterns like x = x + 1) and buffer the
        // write. The buffered value is applied to the service only at commit, so
        // a transaction that fails partway leaves no partial writes behind.
        if txn.is_some() {
            let key = (self.service_net_id_for_name(service_name), var_name);
            enum LockAction {
                Acquire,
                Upgrade,
            }
            let (txn_id, kind) = {
                let t = txn.as_deref().unwrap();
                let kind = if t.locked.contains(&key) {
                    LockAction::Upgrade
                } else {
                    LockAction::Acquire
                };
                (t.id.clone(), kind)
            };
            match kind {
                LockAction::Upgrade => {
                    self.upgrade_to_write_lock(service_name, var_name, &txn_id)?
                }
                LockAction::Acquire => self.acquire_write_lock(service_name, var_name, &txn_id)?,
            }
            if let Some(t) = txn {
                t.locked.insert(key.clone());
                t.written.insert(key.clone(), value.clone());
                // Reads later in the same transaction see the buffered write
                t.read_cache.insert(key, value);
            }
            return Ok(());
        }

        // Non-transactional path: apply the write immediately and propagate.
        if let Some(service) = self.services.get_mut(&service_name) {
            if let Some(var_state) = service.vars.get_mut(&var_name) {
                var_state.value = value;
            } else {
                return Err(EvalError::VarNotFound(format!(
                    "Variable '{}' not found in service '{}'",
                    self.interner.get(var_name),
                    self.interner.get(service_name)
                )));
            }
        } else {
            return Err(EvalError::ServiceNotFound(format!(
                "Service '{}' not found",
                self.interner.get(service_name)
            )));
        }

        // propagate: re-evaluate defs that depend on this var in topo order
        self.propagate(service_name, var_name).await;
        Ok(())
    }

    async fn propagate(&mut self, service_name: Symbol, changed_var: Symbol) {
        // #24: event-driven reactivity over the listener graph. A change to a
        // member notifies its listeners; each local listener recomputes from
        // current values (and cached cross-service deps) and, if it changes,
        // cascades to its own listeners. A listener resolving to another node is
        // notified over the wire.
        let mut worklist: Vec<(Symbol, Symbol)> = vec![(service_name, changed_var)];

        while let Some((svc, member)) = worklist.pop() {
            let listeners: Vec<(ServiceNetId, Symbol)> = self
                .services
                .get(&svc)
                .and_then(|s| s.listeners.get(&member))
                .map(|set| set.iter().cloned().collect())
                .unwrap_or_default();

            for (listener_id, listener_def) in listeners {
                let listener_svc = self
                    .services
                    .iter()
                    .find(|(_, s)| s.id == listener_id)
                    .map(|(name, _)| *name);

                match listener_svc {
                    Some(lsvc) => {
                        if self.recompute_def(lsvc, listener_def).await {
                            worklist.push((lsvc, listener_def));
                        }
                    }
                    None => {
                        self.emit_update(&listener_id, listener_def, svc, member)
                            .await;
                    }
                }
            }
        }
    }

    /// #24: recompute `def` in `svc` from current values, seeding the reactive
    /// cache with this def's cached cross-service deps so MemberAccess resolves
    /// from cache instead of a (possibly remote) lookup. Returns whether the
    /// stored value changed.
    async fn recompute_def(&mut self, svc: Symbol, def: Symbol) -> bool {
        let expr = match self
            .services
            .get(&svc)
            .and_then(|s| s.defs.get(&def))
            .cloned()
        {
            Some(e) => e,
            None => return false,
        };
        let env: Vec<(Symbol, Value)> = self
            .services
            .get(&svc)
            .map(|s| s.vars.iter().map(|(k, v)| (*k, v.value.clone())).collect())
            .unwrap_or_default();
        let cache = self
            .services
            .get(&svc)
            .and_then(|s| s.dep_cache.get(&def))
            .cloned()
            .unwrap_or_default();

        self.reactive_cache = Some(cache);
        let result = eval(
            &expr,
            &env,
            &mut EvalContext {
                manager: self,
                service_name: svc,
                txn: None,
            },
        )
        .await;
        self.reactive_cache = None;

        let value = match result {
            Ok(v) => v,
            Err(e) => {
                log::warn!(
                    "propagation of def '{}' failed: {}",
                    self.interner.get(def),
                    e
                );
                return false;
            }
        };

        match self
            .services
            .get_mut(&svc)
            .and_then(|s| s.vars.get_mut(&def))
        {
            Some(var_state) => {
                let differs = var_state.value != value;
                var_state.value = value;
                differs
            }
            None => false,
        }
    }

    /// #24: fire-and-forget send (no reply awaited).
    async fn send_oneway(&mut self, addr: Address, msg: MeerkatMessage) {
        if let Some(net) = self.network.as_mut() {
            net.handle_command(NetworkCommand::SendMessage { addr, msg })
                .await;
        }
    }

    /// #24: send the current value of `svc.member` to a remote listener.
    async fn emit_update(
        &mut self,
        listener_id: &ServiceNetId,
        listener_def: Symbol,
        svc: Symbol,
        member: Symbol,
    ) {
        let reply_to = match self.listener_addrs.get(listener_id) {
            Some(a) => a.clone(),
            None => return,
        };
        let value = match self
            .services
            .get(&svc)
            .and_then(|s| s.vars.get(&member))
            .map(|vs| vs.value.clone())
        {
            Some(v) => v,
            None => return,
        };
        let net_val = match codec::encode_value(&value, &self.interner) {
            Ok(nv) => nv,
            Err(_) => return,
        };
        let msg = MeerkatMessage::Update {
            listener_service: listener_id.0.clone(),
            listener_def: self.interner.get(listener_def).to_string(),
            source_service: self.interner.get(svc).to_string(),
            member: self.interner.get(member).to_string(),
            value: net_val,
        };
        self.send_oneway(Address::new(&reply_to), msg).await;
    }

    /// #24: subscribe `this_id.listener_def` as a listener on remote `owner.member`.
    async fn subscribe_remote(
        &mut self,
        owner: Symbol,
        member: Symbol,
        this_id: ServiceNetId,
        listener_def: Symbol,
    ) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_SUB_ID: AtomicU64 = AtomicU64::new(1);
        let addr = match self.remote_addr(owner) {
            Ok(a) => a,
            Err(_) => return,
        };
        let reply_to = self.local_reply_addr().await;
        let request_id = NEXT_SUB_ID.fetch_add(1, Ordering::SeqCst);
        let msg = MeerkatMessage::RequestUpdates {
            request_id,
            service: self.interner.get(owner).to_string(),
            member: self.interner.get(member).to_string(),
            listener_service: this_id.0,
            listener_def: self.interner.get(listener_def).to_string(),
            reply_to,
        };
        self.send_oneway(addr, msg).await;
    }

    /// #24 owner side: register a remote listener on `service.member` and reply
    /// with the current value as an initial `Update` so it starts in sync.
    pub async fn handle_request_updates(
        &mut self,
        service: String,
        member: String,
        listener_service: String,
        listener_def: String,
        reply_to: String,
    ) {
        let service_sym = self.interner.insert(&service);
        let member_sym = self.interner.insert(&member);
        let listener_def_sym = self.interner.insert(&listener_def);
        let listener_id = ServiceNetId(listener_service.clone());

        if let Some(svc) = self.services.get_mut(&service_sym) {
            svc.listeners
                .entry(member_sym)
                .or_default()
                .insert((listener_id.clone(), listener_def_sym));
        } else {
            return;
        }
        self.listener_addrs.insert(listener_id, reply_to.clone());

        let current = self
            .services
            .get(&service_sym)
            .and_then(|s| s.vars.get(&member_sym))
            .map(|vs| vs.value.clone());
        if let Some(value) = current {
            if let Ok(net_val) = codec::encode_value(&value, &self.interner) {
                let msg = MeerkatMessage::Update {
                    listener_service,
                    listener_def,
                    source_service: service,
                    member,
                    value: net_val,
                };
                self.send_oneway(Address::new(&reply_to), msg).await;
            }
        }
    }

    /// #24 listener side: a remote member changed (or its initial value). Cache
    /// it, recompute the dependent def from cache, and cascade to its listeners.
    pub async fn handle_update(
        &mut self,
        listener_service: String,
        listener_def: String,
        source_service: String,
        member: String,
        value: crate::net::ast::NetValue,
    ) {
        let value = match codec::decode_value(value, &mut self.interner) {
            Ok(v) => v,
            Err(_) => return,
        };
        let listener_def_sym = self.interner.insert(&listener_def);
        let source_sym = self.interner.insert(&source_service);
        let member_sym = self.interner.insert(&member);

        let listener_svc = self
            .services
            .iter()
            .find(|(_, s)| s.id.0 == listener_service)
            .map(|(name, _)| *name);
        let listener_svc = match listener_svc {
            Some(n) => n,
            None => return,
        };

        if let Some(svc) = self.services.get_mut(&listener_svc) {
            svc.dep_cache
                .entry(listener_def_sym)
                .or_default()
                .insert((source_sym, member_sym), value);
        }

        if self.recompute_def(listener_svc, listener_def_sym).await {
            self.propagate(listener_svc, listener_def_sym).await;
        }
    }

    /// Drain all pending network events and dispatch each to the matching
    /// oneshot channel in pending_replies. Non-matching events are dropped.
    pub fn dispatch_network_events(&mut self) {
        while let Some(n) = self.network.as_mut() {
            let event = n.try_recv_event();
            match event {
                Some(NetworkEvent::MessageReceived { msg, .. }) => {
                    let rid = match &msg {
                        MeerkatMessage::LookupResponse { request_id, .. } => Some(*request_id),
                        MeerkatMessage::LookupError { request_id, .. } => Some(*request_id),
                        MeerkatMessage::ActionResponse { request_id, .. } => Some(*request_id),
                        MeerkatMessage::CommitResponse { request_id, .. } => Some(*request_id),
                        MeerkatMessage::AbortResponse { request_id, .. } => Some(*request_id),
                        MeerkatMessage::WaitParked { request_id, .. } => Some(*request_id),
                        MeerkatMessage::Ping { .. }
                        | MeerkatMessage::Pong { .. }
                        | MeerkatMessage::Announce { .. }
                        | MeerkatMessage::Transaction { .. }
                        | MeerkatMessage::Propagation { .. }
                        | MeerkatMessage::LookupRequest { .. }
                        | MeerkatMessage::ActionRequest { .. }
                        | MeerkatMessage::Commit { .. }
                        | MeerkatMessage::Abort { .. }
                        | MeerkatMessage::RequestUpdates { .. }
                        | MeerkatMessage::Update { .. } => None,
                    };
                    if let Some(request_id) = rid {
                        if let Some(tx) = self.pending_replies.remove(&request_id) {
                            let _ = tx.send(msg);
                        }
                    }
                }
                Some(NetworkEvent::SendFailed { .. }) => {}
                Some(NetworkEvent::PeerConnected { .. }) => {}
                Some(NetworkEvent::PeerDisconnected { .. }) => {}
                None => break,
            }
        }
    }

    /// shared by remote_lookup and remote_action.
    async fn send_and_await_reply(
        &mut self,
        addr: Address,
        msg: MeerkatMessage,
        request_id: u64,
        timeout_msg: String,
    ) -> Result<MeerkatMessage, EvalError> {
        // Send the message
        let net = self.network.as_mut().ok_or_else(|| {
            EvalError::LocalDispatchFailed("No network layer available".to_string())
        })?;
        net.handle_command(NetworkCommand::SendMessage { addr, msg })
            .await;

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
                    match result {
                        // Owner parked our request (wait-die wait): it is alive
                        // and still queued, so reset the timeout, re-register a
                        // fresh reply channel, and keep waiting.
                        Ok(MeerkatMessage::WaitParked { .. }) => {
                            let (ntx, nrx) = oneshot::channel::<MeerkatMessage>();
                            self.pending_replies.insert(request_id, ntx);
                            rx = nrx;
                            timeout
                                .as_mut()
                                .reset(tokio::time::Instant::now() + Duration::from_secs(15));
                        }
                        Ok(msg) => return Ok(msg),
                        Err(_) => {
                            return Err(EvalError::LocalDispatchFailed(
                                "Reply channel closed".to_string(),
                            ))
                        }
                    }
                }
                _ = &mut timeout => {
                    self.pending_replies.remove(&request_id);
                    return Err(EvalError::LocalDispatchFailed(timeout_msg));
                }
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
    }

    /// Retrieve the network address associated with a remote service symbol
    ///
    /// Strips the trailing service slug from the registered service URL.
    ///
    /// Args:
    ///     service (Symbol): The remote service symbol to look up
    ///
    /// Returns:
    ///     Result<Address, EvalError>: The target remote network address
    ///
    /// Raises:
    ///     EvalError::ServiceNotFound: If the remote service is not registered
    fn remote_addr(&self, service: Symbol) -> Result<Address, EvalError> {
        let full_url = self.remote_services.get(&service).ok_or_else(|| {
            EvalError::ServiceNotFound(format!(
                "Remote service '{}' not found",
                self.interner.get(service)
            ))
        })?;
        let service_str = self.interner.get(service);
        let addr_str = full_url.0.trim_end_matches(&format!("/{}", service_str));
        Ok(Address::new(addr_str))
    }

    /// Get our local address with peer ID for use as reply_to
    /// Replaces loopback/unspecified with the actual outbound IP
    async fn local_reply_addr(&mut self) -> String {
        if let Some(addr) = &self.local_address {
            return addr.clone();
        }
        let net = match self.network.as_mut() {
            Some(n) => n,
            None => return String::new(),
        };
        let peer_id = net.local_peer_id();
        let reply = net.handle_command(NetworkCommand::GetLocalAddresses).await;
        let node_ip = self.get_node_ip();
        match reply {
            NetworkReply::LocalAddresses { addrs } => {
                if let Some(addr) = addrs.first() {
                    let addr_str = addr
                        .0
                        .replace("0.0.0.0", &node_ip)
                        .replace("127.0.0.1", &node_ip);
                    format!("{}/p2p/{}", addr_str, peer_id)
                } else {
                    String::new()
                }
            }
            NetworkReply::MessageSent { .. }
            | NetworkReply::ListenSuccess { .. }
            | NetworkReply::Failure(_) => String::new(),
        }
    }

    fn random_node_id() -> u64 {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut h = RandomState::new().build_hasher();
        h.write_u128(nanos);
        h.finish()
    }

    /// Get the local machine's outbound IP address (non-loopback) or loopback fallback
    pub fn get_node_ip(&self) -> String {
        if self.local {
            return "127.0.0.1".to_string();
        }
        use std::net::UdpSocket;
        UdpSocket::bind("0.0.0.0:0")
            .and_then(|s| {
                s.connect("8.8.8.8:80")?;
                s.local_addr()
            })
            .map(|addr| addr.ip().to_string())
            .unwrap_or_else(|_| "127.0.0.1".to_string())
    }

    /// Perform a remote variable lookup over the network
    ///
    /// Sends a lookup query to the node owning the remote service and registers
    /// the local node as a transaction participant.
    ///
    /// Args:
    ///     service (Symbol): The remote service symbol
    ///     member (Symbol): The member/variable symbol within the service
    ///     txn (Option<&mut Transaction>): The active transaction context
    ///
    /// Returns:
    ///     Result<Value, EvalError>: The retrieved value, or a network/timeout error
    ///
    /// Raises:
    ///     EvalError::LocalDispatchFailed: If a timeout or dispatch error occurs
    pub async fn remote_lookup(
        &mut self,
        service: Symbol,
        member: Symbol,
        txn: Option<&mut Transaction>,
    ) -> Result<Value, EvalError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);

        // Remote reads are always served by the owning node, which holds this
        // transaction's buffered writes and read locks. We deliberately do not
        // cache the result on the requesting side: a def's value can change
        // later in the same transaction when a composed action writes one of
        // its dependencies on the owner, so a cached copy would go stale and
        // the def would "stop updating". Re-fetching keeps reads consistent
        // with the owner's buffered state. (Caching provably-immutable reads to
        // save round-trips could be a later optimization.)
        let addr = self.remote_addr(service)?;
        let request_id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let reply_to = self.local_reply_addr().await;
        let shared_tid = txn.as_ref().map(|t| t.id.clone());

        // Inside a transaction, the owning node will acquire and hold a read lock
        // under the shared id. Pre-register it as a participant so commit/abort
        // releases that lock even if the reply is lost.
        if shared_tid.is_some() {
            if let Some(t) = txn {
                t.participants.insert(addr.clone());
            }
        }

        let msg = MeerkatMessage::LookupRequest {
            request_id,
            service: self.interner.get(service).to_string(),
            member: self.interner.get(member).to_string(),
            reply_to,
            txn_id: shared_tid,
        };

        let reply = self
            .send_and_await_reply(
                addr,
                msg,
                request_id,
                format!(
                    "Timeout waiting for remote lookup of '{}.{}'",
                    self.interner.get(service),
                    self.interner.get(member)
                ),
            )
            .await?;

        match reply {
            MeerkatMessage::LookupResponse { value, .. } => {
                let val = codec::decode_value(value, &mut self.interner)
                    .map_err(|e| EvalError::LocalDispatchFailed(e.to_string()))?;
                Ok(val)
            }
            MeerkatMessage::LookupError { error, .. } => Err(EvalError::LocalDispatchFailed(error)),
            MeerkatMessage::Ping { .. }
            | MeerkatMessage::Pong { .. }
            | MeerkatMessage::Announce { .. }
            | MeerkatMessage::Transaction { .. }
            | MeerkatMessage::Propagation { .. }
            | MeerkatMessage::LookupRequest { .. }
            | MeerkatMessage::ActionRequest { .. }
            | MeerkatMessage::ActionResponse { .. }
            | MeerkatMessage::Commit { .. }
            | MeerkatMessage::CommitResponse { .. }
            | MeerkatMessage::Abort { .. }
            | MeerkatMessage::AbortResponse { .. }
            | MeerkatMessage::RequestUpdates { .. }
            | MeerkatMessage::Update { .. }
            | MeerkatMessage::WaitParked { .. } => Err(EvalError::LocalDispatchFailed(
                "Unexpected reply to lookup request".to_string(),
            )),
        }
    }

    /// Participant side: serve a transactional remote read by acquiring and
    /// holding a read lock on the member under the shared transaction id (kept
    /// in pending_txns until commit/abort), accumulating into any state this
    /// node already prepared for the same transaction.
    pub async fn remote_read_participant(
        &mut self,
        service: Symbol,
        member: Symbol,
        tid: TxnId,
    ) -> Result<Value, EvalError> {
        let mut txn = self
            .pending_txns
            .remove(&tid)
            .unwrap_or_else(|| Transaction::new(tid.clone()));
        match self.lookup(member, service, Some(&mut txn)).await {
            Ok(v) => {
                self.pending_txns.insert(tid, txn);
                Ok(v)
            }
            Err(e) => {
                // Wait-die wait: preserve the transaction so the parked read can
                // resume on release; any other failure releases and drops it.
                if matches!(e, EvalError::WaitOn(_, _)) {
                    self.pending_txns.insert(tid, txn);
                    return Err(e);
                }
                // Could not acquire the read lock (e.g. conflict): release any
                // locks taken and do not keep this transaction prepared.
                self.release_locks(&txn.locked, &txn.id);
                Err(e)
            }
        }
    }

    pub async fn remote_action(
        &mut self,
        service_net_id: &ServiceNetId,
        stmts: Vec<ActionStmt>,
        env: Vec<(Symbol, Value)>,
        txn: Option<&mut Transaction>,
    ) -> Result<(), EvalError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ACTION_ID: AtomicU64 = AtomicU64::new(1);

        // Dial the node address embedded in the `ServiceNetId`; send the
        // slug as the service name the remote node uses to find its local
        // service. This works even if the service was never imported
        // into the current scope
        let (addr, slug) = Self::split_service_net_id(service_net_id);
        let request_id = NEXT_ACTION_ID.fetch_add(1, Ordering::SeqCst);
        let reply_to = self.local_reply_addr().await;

        // When part of a transaction, ship its id so the remote node executes
        // under the shared transaction and holds (does not commit) until our
        // commit/abort. Standalone (no txn) keeps the old commit-immediately path.
        let shared_tid = txn.as_ref().map(|t| t.id.clone());

        // Pre-register the participant BEFORE sending. If the request times
        // out or the response is lost after the remote already prepared
        // and grabbed locks, the originator's abort path still iterates
        // `txn.participants` and reaches this node to release them. If
        // the remote never received the request, the `Abort` it gets is a
        // harmless no-op
        if shared_tid.is_some() {
            if let Some(t) = txn {
                t.participants.insert(addr.clone());
            }
        }

        let mut net_stmts = Vec::new();
        for s in &stmts {
            net_stmts.push(
                codec::encode_action_stmt(s, &self.interner)
                    .map_err(|e| EvalError::LocalDispatchFailed(e.to_string()))?,
            );
        }

        let mut net_env = Vec::new();
        for (sym, val) in env {
            let key_str = self.interner.get(sym).to_string();
            let enc_val = codec::encode_value(&val, &self.interner)
                .map_err(|e| EvalError::LocalDispatchFailed(e.to_string()))?;
            net_env.push((key_str, enc_val));
        }

        let msg = MeerkatMessage::ActionRequest {
            request_id,
            service: slug.clone(),
            stmts: net_stmts,
            env: net_env,
            reply_to,
            txn_id: shared_tid,
        };

        let reply = self
            .send_and_await_reply(
                addr.clone(),
                msg,
                request_id,
                format!("Timeout waiting for remote action on service '{}'", slug),
            )
            .await?;

        match reply {
            MeerkatMessage::ActionResponse { success, error, .. } => {
                if success {
                    // Participant already registered above; nothing more to do.
                    Ok(())
                } else {
                    Err(EvalError::LocalDispatchFailed(
                        error.unwrap_or_else(|| "Remote action failed".to_string()),
                    ))
                }
            }
            MeerkatMessage::Ping { .. }
            | MeerkatMessage::Pong { .. }
            | MeerkatMessage::Announce { .. }
            | MeerkatMessage::Transaction { .. }
            | MeerkatMessage::Propagation { .. }
            | MeerkatMessage::LookupRequest { .. }
            | MeerkatMessage::LookupResponse { .. }
            | MeerkatMessage::LookupError { .. }
            | MeerkatMessage::ActionRequest { .. }
            | MeerkatMessage::Commit { .. }
            | MeerkatMessage::CommitResponse { .. }
            | MeerkatMessage::Abort { .. }
            | MeerkatMessage::AbortResponse { .. }
            | MeerkatMessage::RequestUpdates { .. }
            | MeerkatMessage::Update { .. }
            | MeerkatMessage::WaitParked { .. } => Err(EvalError::LocalDispatchFailed(
                "Unexpected reply to action request".to_string(),
            )),
        }
    }

    /// Resolve an in-scope service name to its global `ServiceNetId`
    ///
    /// Callers only resolve names of local services here (remote reads
    /// and actions are routed before reaching this), so this returns
    /// the service's stored, stable ID
    ///
    /// The bare-name fallback is a defensive default for an unknown
    /// name and is not used for genuine remote services, whose
    /// identities travel embedded in their `ActionClosure`s
    pub fn service_net_id_for_name(&self, service_name: Symbol) -> ServiceNetId {
        if let Some(service) = self.services.get(&service_name) {
            service.id.clone()
        } else if let Some(addr) = self.remote_services.get(&service_name) {
            ServiceNetId::new(addr.0.clone())
        } else {
            ServiceNetId::new(self.interner.get(service_name))
        }
    }

    /// Find a local service (mutably) by its `ServiceNetId`
    fn service_by_net_id_mut(&mut self, service_net_id: &ServiceNetId) -> Option<&mut Service> {
        self.services.values_mut().find(|s| &s.id == service_net_id)
    }

    /// Find the in-scope name of a local service from its `ServiceNetId`
    pub fn service_name_for_net_id(&self, service_net_id: &ServiceNetId) -> Option<Symbol> {
        self.services
            .iter()
            .find(|(_, s)| &s.id == service_net_id)
            .map(|(n, _)| *n)
    }

    /// Split a service identity into the dialable node address and the
    /// service slug (its trailing name segment)
    ///
    /// Allows `remote_action` to use the address embedded in an
    /// `ActionClosure`'s `ServiceNetId` rather than requiring the
    /// service to be imported into the current scope
    fn split_service_net_id(service_net_id: &ServiceNetId) -> (Address, String) {
        match service_net_id.0.rfind('/') {
            Some(i) => (
                Address::new(&service_net_id.0[..i]),
                service_net_id.0[i + 1..].to_string(),
            ),
            None => (Address::new(String::new()), service_net_id.0.clone()),
        }
    }

    /// Attempt to acquire a write lock on a service variable
    ///
    /// If lock contention occurs, determines whether the transaction
    /// should wait or die according to the wait-die deadlock prevention
    /// scheme
    ///
    /// Args:
    ///     `service_name` (`Symbol`): The symbol of the service
    ///     `var` (`Symbol`): The symbol of the variable to lock
    ///     `txn_id` (`&TxnId`): The ID of the requesting transaction
    ///
    /// Returns:
    ///     `Result<(), EvalError>`: `Ok` on successful lock acquisition, or an error
    ///
    /// Raises:
    ///     `EvalError::VarNotFound`: If the variable does not exist
    ///     `EvalError::ServiceNotFound`: If the service does not exist
    ///     `EvalError::WaitDieAbort`: If the transaction aborts under wait-die
    ///     `EvalError::WaitOn`: If the transaction must wait for the lock
    fn acquire_write_lock(
        &mut self,
        service_name: Symbol,
        var: Symbol,
        txn_id: &TxnId,
    ) -> Result<(), EvalError> {
        let service = self.services.get_mut(&service_name).ok_or_else(|| {
            EvalError::ServiceNotFound(format!(
                "Service '{}' not found",
                self.interner.get(service_name)
            ))
        })?;
        let var_state = service.vars.get_mut(&var).ok_or_else(|| {
            EvalError::VarNotFound(format!("Variable '{}' not found", self.interner.get(var)))
        })?;
        if var_state.lock.try_write(txn_id) {
            Ok(())
        } else {
            match var_state.lock.wait_die(txn_id) {
                crate::runtime::txn::WaitDie::Die => Err(EvalError::WaitDieAbort(format!(
                    "transaction died contending for write lock on '{}'",
                    self.interner.get(var)
                ))),
                crate::runtime::txn::WaitDie::Wait => Err(EvalError::WaitOn(service_name, var)),
            }
        }
    }

    /// Attempt to acquire a read lock on a service variable
    ///
    /// Multi-readers can share read locks, but will conflict with write locks
    /// Uses wait-die deadlock prevention on contention
    ///
    /// Args:
    ///     `service_name` (`Symbol`): The symbol of the service
    ///     `var` (`Symbol`): The symbol of the variable to lock
    ///     `txn_id` (`&TxnId`): The ID of the requesting transaction
    ///
    /// Returns:
    ///     `Result<(), EvalError>`: `Ok` on successful lock acquisition, or an error
    ///
    /// Raises:
    ///     `EvalError::VarNotFound`: If the variable does not exist
    ///     `EvalError::ServiceNotFound`: If the service does not exist
    ///     `EvalError::WaitDieAbort`: If the transaction aborts under wait-die
    ///     `EvalError::WaitOn`: If the transaction must wait for the lock
    fn acquire_read_lock(
        &mut self,
        service_name: Symbol,
        var: Symbol,
        txn_id: &TxnId,
    ) -> Result<(), EvalError> {
        let service = self.services.get_mut(&service_name).ok_or_else(|| {
            EvalError::ServiceNotFound(format!(
                "Service '{}' not found",
                self.interner.get(service_name)
            ))
        })?;
        let var_state = service.vars.get_mut(&var).ok_or_else(|| {
            EvalError::VarNotFound(format!("Variable '{}' not found", self.interner.get(var)))
        })?;
        if var_state.lock.try_read(txn_id) {
            Ok(())
        } else {
            match var_state.lock.wait_die(txn_id) {
                crate::runtime::txn::WaitDie::Die => Err(EvalError::WaitDieAbort(format!(
                    "transaction died contending for read lock on '{}'",
                    self.interner.get(var)
                ))),
                crate::runtime::txn::WaitDie::Wait => Err(EvalError::WaitOn(service_name, var)),
            }
        }
    }

    /// Upgrade an existing read lock to a write lock on a service variable
    ///
    /// Used for read-then-write patterns in transactions to avoid conflicts
    ///
    /// Args:
    ///     `service_name` (`Symbol`): The symbol of the service
    ///     `var` (`Symbol`): The symbol of the variable to lock
    ///     `txn_id` (`&TxnId`): The ID of the requesting transaction
    ///
    /// Returns:
    ///     `Result<(), EvalError>`: `Ok` on successful lock upgrade, or an error
    ///
    /// Raises:
    ///     `EvalError::VarNotFound`: If the variable does not exist
    ///     `EvalError::ServiceNotFound`: If the service does not exist
    ///     `EvalError::WaitDieAbort`: If the transaction aborts under wait-die
    ///     `EvalError::WaitOn`: If the transaction must wait for the lock
    fn upgrade_to_write_lock(
        &mut self,
        service_name: Symbol,
        var: Symbol,
        txn_id: &TxnId,
    ) -> Result<(), EvalError> {
        let service = self.services.get_mut(&service_name).ok_or_else(|| {
            EvalError::ServiceNotFound(format!(
                "Service '{}' not found",
                self.interner.get(service_name)
            ))
        })?;
        let var_state = service.vars.get_mut(&var).ok_or_else(|| {
            EvalError::VarNotFound(format!("Variable '{}' not found", self.interner.get(var)))
        })?;
        if var_state.lock.upgrade_to_write(txn_id) {
            Ok(())
        } else {
            match var_state.lock.wait_die(txn_id) {
                crate::runtime::txn::WaitDie::Die => Err(EvalError::WaitDieAbort(format!(
                    "transaction died contending to upgrade lock on '{}'",
                    self.interner.get(var)
                ))),
                crate::runtime::txn::WaitDie::Wait => Err(EvalError::WaitOn(service_name, var)),
            }
        }
    }

    /// Release all locks held by `txn_id` on the given variables
    fn release_locks(&mut self, locked: &HashSet<(ServiceNetId, Symbol)>, txn_id: &TxnId) {
        for (sid, var) in locked {
            if let Some(service) = self.service_by_net_id_mut(sid) {
                if let Some(var_state) = service.vars.get_mut(var) {
                    var_state.lock.release(txn_id);
                }
            }
        }
    }

    /// Execute action statements as a transaction with lazy lock
    /// acquisition
    ///
    /// Locks are acquired on demand as each variable is first read or
    /// written during execution (inside `lookup` and `assign`), rather
    /// than upfront.
    /// This handles actions invoked via function calls and conditional
    /// branches, where the set of accessed variables cannot be
    /// determined statically.
    /// Read values are cached in the transaction to avoid re-fetching
    /// (which also avoids redundant network round-trips for remote
    /// reads)
    ///
    /// On completion, a commit records `latest_write_txn` for written
    /// variables, then all locks are released (always, even on error)
    ///
    /// If a lock cannot be acquired, wait-die deadlock prevention
    /// determines whether the transaction waits or dies
    pub async fn execute_action_with_txn(
        &mut self,
        service_name: Symbol,
        stmts: &[ActionStmt],
        initial_env: &[(Symbol, Value)],
    ) -> Result<(), EvalError> {
        const MAX_WAIT_DIE_RETRIES: u32 = 10;
        let mut txn_id = TxnId::new(self.node_id);

        loop {
            let mut txn = Transaction::new(txn_id.clone());

            let mut env: Vec<(Symbol, Value)> = initial_env.to_vec();
            let mut exec_error: Option<EvalError> = None;
            for stmt in stmts {
                match execute(stmt, &env, self, service_name, Some(&mut txn)).await {
                    Ok(ExecuteEffect::Binding(name, val)) => env.push((name, val)),
                    Ok(_) => {}
                    Err(e) => {
                        exec_error = Some(e);
                        break;
                    }
                }
            }

            if matches!(exec_error, Some(EvalError::WaitDieAbort(_))) {
                for addr in txn.participants.iter().cloned().collect::<Vec<_>>() {
                    self.send_abort(addr, &txn.id).await;
                }
                self.release_locks(&txn.locked, &txn.id);
                if txn_id.iteration < MAX_WAIT_DIE_RETRIES {
                    txn_id = txn_id.retry();
                    continue;
                }
                return Err(exec_error.unwrap());
            }

            if exec_error.is_none() {
                self.apply_committed_writes(&txn).await;
                for addr in txn.participants.iter().cloned().collect::<Vec<_>>() {
                    let _ = self.send_commit(addr, &txn.id).await;
                }
            } else {
                for addr in txn.participants.iter().cloned().collect::<Vec<_>>() {
                    self.send_abort(addr, &txn.id).await;
                }
            }

            self.release_locks(&txn.locked, &txn.id);

            return match exec_error {
                Some(e) => Err(e),
                None => Ok(()),
            };
        }
    }

    /// Apply a transaction's buffered writes to the owning services, record
    /// the writing transaction, and propagate to dependent definitions
    ///
    /// Shared by local commit and by a participant committing on a remote
    /// `Commit` message
    ///
    /// Infallible: once we are applying writes the transaction is
    /// committed, so there is no going back. Propagation is best-effort
    async fn apply_committed_writes(&mut self, txn: &Transaction) {
        let writes: Vec<((ServiceNetId, Symbol), Value)> = txn
            .written
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let txn_id = txn.id.clone();
        for ((sid, var), value) in &writes {
            if let Some(service) = self.service_by_net_id_mut(sid) {
                if let Some(var_state) = service.vars.get_mut(var) {
                    var_state.value = value.clone();
                    var_state.latest_write_txn = Some(txn_id.clone());
                }
            }
        }
        for ((sid, var), _) in &writes {
            if let Some(name) = self.service_name_for_net_id(sid) {
                self.propagate(name, *var).await;
            }
        }
    }

    /// Participant side: execute a composed action under a shared transaction
    /// ID received from the originator, then hold the transaction (locks and
    /// buffered writes) in `pending_txns` until a `Commit` or `Abort` arrives
    ///
    /// Does not commit
    pub async fn execute_action_participant(
        &mut self,
        service_name: Symbol,
        stmts: &[ActionStmt],
        initial_env: &[(Symbol, Value)],
        tid: TxnId,
    ) -> Result<(), EvalError> {
        let mut txn = self
            .pending_txns
            .remove(&tid)
            .unwrap_or_else(|| Transaction::new(tid.clone()));
        let mut env: Vec<(Symbol, Value)> = initial_env.to_vec();
        let mut exec_error: Option<EvalError> = None;
        for stmt in stmts {
            match execute(stmt, &env, self, service_name, Some(&mut txn)).await {
                Ok(ExecuteEffect::Binding(name, val)) => env.push((name, val)),
                Ok(_) => {}
                Err(e) => {
                    exec_error = Some(e);
                    break;
                }
            }
        }
        if let Some(e) = exec_error {
            if matches!(e, EvalError::WaitOn(_, _)) {
                self.pending_txns.insert(tid, txn);
                return Err(e);
            }
            self.release_locks(&txn.locked, &txn.id);
            return Err(e);
        }
        self.pending_txns.insert(tid, txn);
        Ok(())
    }

    /// Participant side: apply and release a held transaction on `Commit`
    pub async fn commit_participant(
        &mut self,
        tid: &TxnId,
    ) -> Result<HashSet<(ServiceNetId, Symbol)>, EvalError> {
        if let Some(txn) = self.pending_txns.remove(tid) {
            let freed = txn.locked.clone();
            self.apply_committed_writes(&txn).await;
            self.release_locks(&txn.locked, &txn.id);
            let mut forward_err = None;
            for addr in txn.participants.iter().cloned().collect::<Vec<_>>() {
                if let Err(e) = self.send_commit(addr, tid).await {
                    forward_err = Some(e);
                }
            }
            match forward_err {
                Some(e) => Err(e),
                None => Ok(freed),
            }
        } else {
            Ok(HashSet::new())
        }
    }

    /// Participant side: discard and release a held transaction on `Abort`, and
    /// forward the abort down the chain to any sub-participants
    pub async fn abort_participant(&mut self, tid: &TxnId) -> HashSet<(ServiceNetId, Symbol)> {
        if let Some(txn) = self.pending_txns.remove(tid) {
            let freed = txn.locked.clone();
            self.release_locks(&txn.locked, &txn.id);
            for addr in txn.participants.iter().cloned().collect::<Vec<_>>() {
                self.send_abort(addr, tid).await;
            }
            freed
        } else {
            HashSet::new()
        }
    }

    /// Originator side: ask a participant to commit, awaiting its acknowledgement
    async fn send_commit(&mut self, addr: Address, tid: &TxnId) -> Result<(), EvalError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_COMMIT_ID: AtomicU64 = AtomicU64::new(1);
        let request_id = NEXT_COMMIT_ID.fetch_add(1, Ordering::SeqCst);
        let reply_to = self.local_reply_addr().await;
        let msg = MeerkatMessage::Commit {
            request_id,
            txn_id: tid.clone(),
            reply_to,
        };
        let reply = self
            .send_and_await_reply(
                addr,
                msg,
                request_id,
                "Timeout waiting for commit acknowledgement".to_string(),
            )
            .await?;
        match reply {
            MeerkatMessage::CommitResponse { success, error, .. } => {
                if success {
                    Ok(())
                } else {
                    Err(EvalError::LocalDispatchFailed(
                        error.unwrap_or_else(|| "Participant commit failed".to_string()),
                    ))
                }
            }
            MeerkatMessage::Ping { .. }
            | MeerkatMessage::Pong { .. }
            | MeerkatMessage::Announce { .. }
            | MeerkatMessage::Transaction { .. }
            | MeerkatMessage::Propagation { .. }
            | MeerkatMessage::LookupRequest { .. }
            | MeerkatMessage::LookupResponse { .. }
            | MeerkatMessage::LookupError { .. }
            | MeerkatMessage::ActionRequest { .. }
            | MeerkatMessage::ActionResponse { .. }
            | MeerkatMessage::Commit { .. }
            | MeerkatMessage::Abort { .. }
            | MeerkatMessage::AbortResponse { .. }
            | MeerkatMessage::RequestUpdates { .. }
            | MeerkatMessage::Update { .. }
            | MeerkatMessage::WaitParked { .. } => Err(EvalError::LocalDispatchFailed(
                "Unexpected reply to commit".to_string(),
            )),
        }
    }

    /// Originator side: tell a participant to abort, awaiting acknowledgement
    /// so its locks are released before we return (and the process may exit).
    async fn send_abort(&mut self, addr: Address, tid: &TxnId) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ABORT_ID: AtomicU64 = AtomicU64::new(1);
        let request_id = NEXT_ABORT_ID.fetch_add(1, Ordering::SeqCst);
        let reply_to = self.local_reply_addr().await;
        let msg = MeerkatMessage::Abort {
            request_id,
            txn_id: tid.clone(),
            reply_to,
        };
        // We await the ack so that in the normal case the participant's locks
        // are released before we return. If the ack times out the participant
        // may still hold locks; durable abort retries and error reporting are
        // tracked under issue #54.
        let _ = self
            .send_and_await_reply(
                addr,
                msg,
                request_id,
                "Timeout waiting for abort acknowledgement".to_string(),
            )
            .await;
    }

    pub async fn execute_action(
        &mut self,
        service_name: Symbol,
        stmts: &[ActionStmt],
    ) -> Result<(), EvalError> {
        self.execute_action_with_txn(service_name, stmts, &[]).await
    }

    pub async fn execute_action_with_env(
        &mut self,
        service_name: Symbol,
        stmts: &[ActionStmt],
        initial_env: &[(Symbol, Value)],
    ) -> Result<(), EvalError> {
        self.execute_action_with_txn(service_name, stmts, initial_env)
            .await
    }
}

impl Default for Manager {
    fn default() -> Self {
        Self::new(Interner::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Decl, Expr, Value};

    // #24: cross_service_deps pulls out exactly the (service, member) symbols
    // referenced via MemberAccess, and nothing for a purely local expression.
    #[test]
    fn test_cross_service_deps_extraction() {
        let tc = TestContext::new();
        // z = s1.y + 2  ->  {(s1, y)}
        let z_expr = Expr::Binop {
            op: crate::ast::BinOp::Add,
            expr1: Box::new(Expr::MemberAccess {
                service_name: tc.s1,
                member_name: tc.y,
            }),
            expr2: Box::new(Expr::Literal {
                val: Value::Number { val: 2 },
            }),
        };
        assert_eq!(
            z_expr.cross_service_deps(),
            std::collections::HashSet::from([(tc.s1, tc.y)])
        );
        // y = x + 1  ->  {} (no cross-service references)
        let y_expr = Expr::Binop {
            op: crate::ast::BinOp::Add,
            expr1: Box::new(Expr::Variable { name: tc.x }),
            expr2: Box::new(Expr::Literal {
                val: Value::Number { val: 1 },
            }),
        };
        assert!(y_expr.cross_service_deps().is_empty());
    }

    // #24: a def in s2 that reads s1.y updates eagerly when s1.x changes,
    // driven by the listener cascade rather than a lazy re-lookup.
    #[tokio::test]
    async fn test_cross_service_def_updates_eagerly() {
        let mut tc = TestContext::new();
        let z = tc.manager.interner.insert("z");

        // service s1 { var x = 1; pub def y = x + 1; }
        let s1_decls = vec![
            Decl::VarDecl {
                name: tc.x,
                val: Expr::Literal {
                    val: Value::Number { val: 1 },
                },
            },
            Decl::DefDecl {
                name: tc.y,
                val: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Variable { name: tc.x }),
                    expr2: Box::new(Expr::Literal {
                        val: Value::Number { val: 1 },
                    }),
                },
                is_pub: true,
            },
        ];
        tc.manager.create_service(tc.s1, s1_decls).await.unwrap();

        // service s2 { pub def z = s1.y + 2; }
        let s2_decls = vec![Decl::DefDecl {
            name: z,
            val: Expr::Binop {
                op: crate::ast::BinOp::Add,
                expr1: Box::new(Expr::MemberAccess {
                    service_name: tc.s1,
                    member_name: tc.y,
                }),
                expr2: Box::new(Expr::Literal {
                    val: Value::Number { val: 2 },
                }),
            },
            is_pub: true,
        }];
        tc.manager.create_service(tc.s2, s2_decls).await.unwrap();

        // initial z = (1 + 1) + 2 = 4
        assert_eq!(
            tc.manager
                .services
                .get(&tc.s2)
                .unwrap()
                .vars
                .get(&z)
                .unwrap()
                .value,
            Value::Number { val: 4 }
        );

        // s2.z is registered as a listener on s1.y
        let on_y = tc
            .manager
            .services
            .get(&tc.s1)
            .unwrap()
            .listeners
            .get(&tc.y)
            .cloned()
            .unwrap_or_default();
        assert!(
            on_y.iter().any(|(_, d)| *d == z),
            "s2.z should be registered as a listener on s1.y"
        );

        // s1.x = 4  ->  s1.y = 5  ->  s2.z = 7, eagerly via the cascade
        tc.manager
            .assign(tc.s1, tc.x, Value::Number { val: 4 }, None)
            .await
            .unwrap();

        assert_eq!(
            tc.manager
                .services
                .get(&tc.s2)
                .unwrap()
                .vars
                .get(&z)
                .unwrap()
                .value,
            Value::Number { val: 7 },
            "s2.z should update eagerly through the cross-service listener cascade"
        );
    }

    // #24: handle_update caches a pushed remote value and recomputes the
    // dependent def FROM THE CACHE, not from a fresh lookup of the local value.
    #[tokio::test]
    async fn test_handle_update_recomputes_from_cache() {
        let mut tc = TestContext::new();
        let z = tc.manager.interner.insert("z");

        let s1_decls = vec![
            Decl::VarDecl {
                name: tc.x,
                val: Expr::Literal {
                    val: Value::Number { val: 1 },
                },
            },
            Decl::DefDecl {
                name: tc.y,
                val: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Variable { name: tc.x }),
                    expr2: Box::new(Expr::Literal {
                        val: Value::Number { val: 1 },
                    }),
                },
                is_pub: true,
            },
        ];
        tc.manager.create_service(tc.s1, s1_decls).await.unwrap();

        let s2_decls = vec![Decl::DefDecl {
            name: z,
            val: Expr::Binop {
                op: crate::ast::BinOp::Add,
                expr1: Box::new(Expr::MemberAccess {
                    service_name: tc.s1,
                    member_name: tc.y,
                }),
                expr2: Box::new(Expr::Literal {
                    val: Value::Number { val: 2 },
                }),
            },
            is_pub: true,
        }];
        tc.manager.create_service(tc.s2, s2_decls).await.unwrap();

        let s2_id = tc.manager.services.get(&tc.s2).unwrap().id.0.clone();
        let net_val =
            codec::encode_value(&Value::Number { val: 10 }, &tc.manager.interner).unwrap();

        // simulate a remote Update saying s1.y = 10
        tc.manager
            .handle_update(
                s2_id,
                "z".to_string(),
                "s1".to_string(),
                "y".to_string(),
                net_val,
            )
            .await;

        // recomputed from the cached 10 (not s1's local y of 2): 10 + 2 = 12
        assert_eq!(
            tc.manager
                .services
                .get(&tc.s2)
                .unwrap()
                .vars
                .get(&z)
                .unwrap()
                .value,
            Value::Number { val: 12 }
        );
    }

    // #24: handle_request_updates registers a remote listener and records its
    // reply address (the initial Update send is a no-op without a network).
    #[tokio::test]
    async fn test_handle_request_updates_registers_listener() {
        let mut tc = TestContext::new();
        let s1_decls = vec![
            Decl::VarDecl {
                name: tc.x,
                val: Expr::Literal {
                    val: Value::Number { val: 1 },
                },
            },
            Decl::DefDecl {
                name: tc.y,
                val: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Variable { name: tc.x }),
                    expr2: Box::new(Expr::Literal {
                        val: Value::Number { val: 1 },
                    }),
                },
                is_pub: true,
            },
        ];
        tc.manager.create_service(tc.s1, s1_decls).await.unwrap();

        tc.manager
            .handle_request_updates(
                "s1".to_string(),
                "y".to_string(),
                "remote-s2-id".to_string(),
                "z".to_string(),
                "/ip4/1.2.3.4/tcp/9".to_string(),
            )
            .await;

        let z = tc.manager.interner.insert("z");
        let on_y = tc
            .manager
            .services
            .get(&tc.s1)
            .unwrap()
            .listeners
            .get(&tc.y)
            .cloned()
            .unwrap_or_default();
        assert!(
            on_y.iter().any(|(id, d)| id.0 == "remote-s2-id" && *d == z),
            "remote s2.z should be registered as a listener on s1.y"
        );
        assert_eq!(
            tc.manager
                .listener_addrs
                .get(&ServiceNetId("remote-s2-id".to_string()))
                .map(|a| a.as_str()),
            Some("/ip4/1.2.3.4/tcp/9")
        );
    }

    struct TestContext {
        manager: Manager,
        foo: Symbol,
        x: Symbol,
        y: Symbol,
        f: Symbol,
        s1: Symbol,
        s2: Symbol,
        w: Symbol,
        bump: Symbol,
        nonexistent: Symbol,
    }

    impl TestContext {
        fn new() -> Self {
            let mut manager = Manager::default();
            let foo = manager.interner.insert("foo");
            let x = manager.interner.insert("x");
            let y = manager.interner.insert("y");
            let f = manager.interner.insert("f");
            let s1 = manager.interner.insert("s1");
            let s2 = manager.interner.insert("s2");
            let w = manager.interner.insert("w");
            let bump = manager.interner.insert("bump");
            let nonexistent = manager.interner.insert("nonexistent");
            Self {
                manager,
                foo,
                x,
                y,
                f,
                s1,
                s2,
                w,
                bump,
                nonexistent,
            }
        }
    }

    #[tokio::test]
    async fn test_create_service_with_var() {
        let mut tc = TestContext::new();
        let decls = vec![Decl::VarDecl {
            name: tc.x,
            ty: None,
            val: Expr::Literal {
                val: Value::Int { val: 1 },
            },
        }];
        tc.manager.create_service(tc.foo, decls).await.unwrap();
        let result = tc.manager.lookup(tc.x, tc.foo, None).await.unwrap();
        assert_eq!(result, Value::Int { val: 1 });
    }

    #[tokio::test]
    async fn test_create_service_with_def() {
        let mut tc = TestContext::new();
        let decls = vec![
            Decl::VarDecl {
                name: tc.x,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 2 },
                },
            },
            Decl::DefDecl {
                name: tc.f,
                ty: None,
                val: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Variable { name: tc.x }),
                    expr2: Box::new(Expr::Literal {
                        val: Value::Int { val: 3 },
                    }),
                },
                is_pub: true,
            },
        ];
        tc.manager.create_service(tc.foo, decls).await.unwrap();
        let result = tc.manager.lookup(tc.f, tc.foo, None).await.unwrap();
        assert_eq!(result, Value::Int { val: 5 });
    }

    #[tokio::test]
    async fn test_lookup_missing_var_returns_error() {
        let mut tc = TestContext::new();
        tc.manager.create_service(tc.foo, vec![]).await.unwrap();
        let result = tc.manager.lookup(tc.nonexistent, tc.foo, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_def_updates_after_var_change() {
        let mut tc = TestContext::new();
        let decls = vec![
            Decl::VarDecl {
                name: tc.x,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 1 },
                },
            },
            Decl::DefDecl {
                name: tc.f,
                ty: None,
                val: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Variable { name: tc.x }),
                    expr2: Box::new(Expr::Literal {
                        val: Value::Int { val: 10 },
                    }),
                },
                is_pub: true,
            },
        ];
        tc.manager.create_service(tc.foo, decls).await.unwrap();

        // f should be 11 initially
        let result = tc.manager.lookup(tc.f, tc.foo, None).await.unwrap();
        assert_eq!(result, Value::Int { val: 11 });

        // update x to 5, f should become 15
        tc.manager
            .assign(tc.foo, tc.x, Value::Int { val: 5 }, None)
            .await
            .unwrap();
        let result = tc.manager.lookup(tc.f, tc.foo, None).await.unwrap();
        assert_eq!(result, Value::Int { val: 15 });
    }

    // Helper: service with a single var x = 0
    async fn manager_with_x() -> TestContext {
        let mut tc = TestContext::new();
        let decls = vec![Decl::VarDecl {
            name: tc.x,
            ty: None,
            val: Expr::Literal {
                val: Value::Int { val: 0 },
            },
        }];
        tc.manager.create_service(tc.foo, decls).await.unwrap();
        tc
    }

    fn x_state(tc: &TestContext) -> &VarState {
        tc.manager
            .services
            .get(&tc.foo)
            .unwrap()
            .vars
            .get(&tc.x)
            .unwrap()
    }

    fn assert_x_unlocked(tc: &TestContext) {
        assert!(matches!(
            &x_state(tc).lock,
            crate::runtime::txn::VarLock::Unlocked
        ));
    }

    #[tokio::test]
    async fn test_txn_read_then_write_upgrades_lock() {
        // `x = x + 1` reads `x` (read lock) then writes `x` (must upgrade
        // to write lock)
        // This is the read-then-write pattern that the old upfront
        // analysis mishandled
        let mut tc = manager_with_x().await;
        let stmts = vec![ActionStmt::Assign {
            name: tc.x,
            expr: Expr::Binop {
                op: crate::ast::BinOp::Add,
                expr1: Box::new(Expr::Variable { name: tc.x }),
                expr2: Box::new(Expr::Literal {
                    val: Value::Int { val: 1 },
                }),
            },
        }];
        tc.manager.execute_action(tc.foo, &stmts).await.unwrap();
        let result = tc.manager.lookup(tc.x, tc.foo, None).await.unwrap();
        assert_eq!(result, Value::Int { val: 1 });
    }

    #[tokio::test]
    async fn test_txn_locks_released_between_transactions() {
        // Locks must be released after a transaction, so a second
        // transaction can acquire them. Running `x = x + 1` twice
        // should yield `x == 2`
        let mut tc = manager_with_x().await;
        let stmts = vec![ActionStmt::Assign {
            name: tc.x,
            expr: Expr::Binop {
                op: crate::ast::BinOp::Add,
                expr1: Box::new(Expr::Variable { name: tc.x }),
                expr2: Box::new(Expr::Literal {
                    val: Value::Int { val: 1 },
                }),
            },
        }];
        tc.manager.execute_action(tc.foo, &stmts).await.unwrap();
        tc.manager.execute_action(tc.foo, &stmts).await.unwrap();
        let result = tc.manager.lookup(tc.x, tc.foo, None).await.unwrap();
        assert_eq!(result, Value::Int { val: 2 });
    }

    #[tokio::test]
    async fn test_txn_var_unlocked_after_commit() {
        // After a transaction completes, the variable's lock should
        // be `Unlocked`
        let mut tc = manager_with_x().await;
        let stmts = vec![ActionStmt::Assign {
            name: tc.x,
            expr: Expr::Literal {
                val: Value::Int { val: 42 },
            },
        }];
        tc.manager.execute_action(tc.foo, &stmts).await.unwrap();
        assert_x_unlocked(&tc);
    }

    #[tokio::test]
    async fn test_txn_successful_write_updates_value_and_latest_write_txn() {
        // A successful transaction commits its buffered write and records
        // the transaction as the latest writer for that variable
        let mut tc = manager_with_x().await;
        let stmts = vec![ActionStmt::Assign {
            name: tc.x,
            expr: Expr::Literal {
                val: Value::Int { val: 42 },
            },
        }];

        tc.manager.execute_action(tc.foo, &stmts).await.unwrap();

        let state = x_state(&tc);
        assert_eq!(state.value, Value::Int { val: 42 });
        assert!(state.latest_write_txn.is_some());
    }

    #[tokio::test]
    async fn test_txn_nested_do_reuses_transaction() {
        // A nested `do` (an action invoking another action) must reuse
        // the same transaction, not start a fresh one. The inner write
        // to `x` should commit and all locks should be released afterward
        // This guards the bug where nested execution clobbered the outer
        // transaction's lock tracking
        let mut tc = manager_with_x().await;
        // outer action: `do` (action { `x` = `x` + 1 })
        let inner = Expr::Action(vec![ActionStmt::Assign {
            name: tc.x,
            expr: Expr::Binop {
                op: crate::ast::BinOp::Add,
                expr1: Box::new(Expr::Variable { name: tc.x }),
                expr2: Box::new(Expr::Literal {
                    val: Value::Int { val: 1 },
                }),
            },
        }]);
        let stmts = vec![ActionStmt::Do(inner)];
        tc.manager.execute_action(tc.foo, &stmts).await.unwrap();

        // inner write took effect
        let result = tc.manager.lookup(tc.x, tc.foo, None).await.unwrap();
        // and the lock was released
        assert_eq!(result, Value::Int { val: 1 });
        assert_x_unlocked(&tc);
    }

    #[tokio::test]
    async fn test_txn_failed_transaction_leaves_no_partial_writes() {
        // A transaction that fails partway must leave no partial writes:
        // writes are buffered and applied only on a successful commit
        // Here the first statement writes `x`, the second fails
        // (asserting `false`), so `x` must stay unchanged
        let mut tc = manager_with_x().await;
        let stmts = vec![
            ActionStmt::Assign {
                name: tc.x,
                expr: Expr::Literal {
                    val: Value::Int { val: 99 },
                },
            },
            ActionStmt::Assert(
                Expr::Literal {
                    val: Value::Bool { val: false },
                },
                "false".to_string(),
            ),
        ];
        let result = tc.manager.execute_action(tc.foo, &stmts).await;
        assert!(result.is_err());
        // `x` must remain 0 — the buffered write to 99 was never committed
        let x = tc.manager.lookup(tc.x, tc.foo, None).await.unwrap();
        // and the lock was released
        assert_eq!(x, Value::Int { val: 0 });
        assert_x_unlocked(&tc);
    }

    #[tokio::test]
    async fn test_txn_failed_transaction_preserves_previous_latest_write_txn() {
        // A failed transaction must not update either committed state
        // field: the value and latest writer should remain from the last
        // successful commit
        let mut tc = manager_with_x().await;
        let successful_write = vec![ActionStmt::Assign {
            name: tc.x,
            expr: Expr::Literal {
                val: Value::Int { val: 1 },
            },
        }];
        tc.manager
            .execute_action(tc.foo, &successful_write)
            .await
            .unwrap();
        let previous_txn = x_state(&tc).latest_write_txn.clone();
        assert!(previous_txn.is_some());

        let failing_write = vec![
            ActionStmt::Assign {
                name: tc.x,
                expr: Expr::Literal {
                    val: Value::Int { val: 99 },
                },
            },
            ActionStmt::Assert(
                Expr::Literal {
                    val: Value::Bool { val: false },
                },
                "false".to_string(),
            ),
        ];

        let result = tc.manager.execute_action(tc.foo, &failing_write).await;

        assert!(result.is_err());
        let state = x_state(&tc);
        assert_eq!(state.value, Value::Int { val: 1 });
        assert_eq!(state.latest_write_txn, previous_txn);
        assert_x_unlocked(&tc);
    }

    #[tokio::test]
    async fn test_txn_read_lock_released_after_failure() {
        // If a transaction fails after a read, its read lock must still
        // be released
        let mut tc = manager_with_x().await;
        let last_txn = x_state(&tc).latest_write_txn.clone();
        let stmts = vec![ActionStmt::Assert(
            Expr::Variable { name: tc.x },
            "x".to_string(),
        )];

        let result = tc.manager.execute_action(tc.foo, &stmts).await;

        assert!(result.is_err());
        assert_eq!(x_state(&tc).value, Value::Int { val: 0 });
        // NOTE: changed this test case to check that the latest_write_txn is the txn that created
        // the service and not none (as it was previously). Since this is changing a test case,
        // please make sure to review it
        assert_eq!(x_state(&tc).latest_write_txn, last_txn);
        assert_x_unlocked(&tc);
    }

    #[tokio::test]
    async fn test_txn_cross_service_composition() {
        // A transaction beginning in `s1` composes an action defined in
        // `s2` (the example from issue #44). Both services' writes must
        // commit under the one transaction, and the `(service_net_id, var)`
        // keying must keep them distinct
        let mut tc = TestContext::new();
        // s2 owns `w` and an action that bumps it
        let bump = Expr::Action(vec![ActionStmt::Assign {
            name: tc.w,
            expr: Expr::Binop {
                op: crate::ast::BinOp::Add,
                expr1: Box::new(Expr::Variable { name: tc.w }),
                expr2: Box::new(Expr::Literal {
                    val: Value::Int { val: 5 },
                }),
            },
        }]);
        tc.manager
            .create_service(
                tc.s2,
                vec![
                    Decl::VarDecl {
                        name: tc.w,
                        ty: None,
                        val: Expr::Literal {
                            val: Value::Int { val: 10 },
                        },
                    },
                    Decl::DefDecl {
                        name: tc.bump,
                        ty: None,
                        val: bump,
                        is_pub: true,
                    },
                ],
            )
            .await
            .unwrap();
        // s1 owns `x`
        tc.manager
            .create_service(
                tc.s1,
                vec![Decl::VarDecl {
                    name: tc.x,
                    ty: None,
                    val: Expr::Literal {
                        val: Value::Int { val: 0 },
                    },
                }],
            )
            .await
            .unwrap();

        // Transaction on `s1`: `x` = `x` + 1; `do` `s2.bump`
        let stmts = vec![
            ActionStmt::Assign {
                name: tc.x,
                expr: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Variable { name: tc.x }),
                    expr2: Box::new(Expr::Literal {
                        val: Value::Int { val: 1 },
                    }),
                },
            },
            ActionStmt::Do(Expr::MemberAccess {
                service_name: tc.s2,
                member_name: tc.bump,
            }),
        ];
        tc.manager.execute_action(tc.s1, &stmts).await.unwrap();

        // Both services' writes committed
        assert_eq!(
            tc.manager.lookup(tc.x, tc.s1, None).await.unwrap(),
            Value::Int { val: 1 }
        );
        assert_eq!(
            tc.manager.lookup(tc.w, tc.s2, None).await.unwrap(),
            Value::Int { val: 15 }
        );
        // Locks released on both services
        assert!(matches!(
            tc.manager
                .services
                .get(&tc.s1)
                .unwrap()
                .vars
                .get(&tc.x)
                .unwrap()
                .lock,
            crate::runtime::txn::VarLock::Unlocked
        ));
        assert!(matches!(
            tc.manager
                .services
                .get(&tc.s2)
                .unwrap()
                .vars
                .get(&tc.w)
                .unwrap()
                .lock,
            crate::runtime::txn::VarLock::Unlocked
        ));
    }

    #[tokio::test]
    async fn test_wait_die_younger_dies_at_acquire() {
        // Wait-die: a younger transaction contending for a lock held by
        // an older transaction dies (abort) rather than acquiring it
        let mut tc = TestContext::new();
        tc.manager
            .create_service(
                tc.s1,
                vec![Decl::VarDecl {
                    name: tc.x,
                    ty: None,
                    val: Expr::Literal {
                        val: Value::Int { val: 0 },
                    },
                }],
            )
            .await
            .unwrap();
        let older = crate::runtime::txn::TxnId {
            timestamp: 1,
            node_id: 1,
            iteration: 0,
        };
        tc.manager
            .services
            .get_mut(&tc.s1)
            .unwrap()
            .vars
            .get_mut(&tc.x)
            .unwrap()
            .lock = crate::runtime::txn::VarLock::WriteLocked(older);
        let younger = crate::runtime::txn::TxnId {
            timestamp: u128::MAX,
            node_id: 1,
            iteration: 0,
        };
        let result = tc.manager.acquire_write_lock(tc.s1, tc.x, &younger);
        assert!(matches!(result, Err(EvalError::WaitDieAbort(_))));
    }

    #[tokio::test]
    async fn test_wait_die_older_takes_wait_path() {
        // Wait-die: an older transaction contending for a lock held by
        // a younger transaction takes the wait path, surfaced as
        // `WaitOn` carrying the contended `(service, var)` so the owner
        // can park the request
        let mut tc = TestContext::new();
        tc.manager
            .create_service(
                tc.s1,
                vec![Decl::VarDecl {
                    name: tc.x,
                    ty: None,
                    val: Expr::Literal {
                        val: Value::Int { val: 0 },
                    },
                }],
            )
            .await
            .unwrap();
        let younger = crate::runtime::txn::TxnId {
            timestamp: u128::MAX,
            node_id: 1,
            iteration: 0,
        };
        tc.manager
            .services
            .get_mut(&tc.s1)
            .unwrap()
            .vars
            .get_mut(&tc.x)
            .unwrap()
            .lock = crate::runtime::txn::VarLock::WriteLocked(younger);
        let older = crate::runtime::txn::TxnId {
            timestamp: 1,
            node_id: 1,
            iteration: 0,
        };
        let result = tc.manager.acquire_write_lock(tc.s1, tc.x, &older);
        assert!(matches!(result, Err(EvalError::WaitOn(_, _))));
    }

    #[tokio::test]
    async fn test_wait_die_action_dies_and_retries() {
        let mut tc = TestContext::new();
        tc.manager
            .create_service(
                tc.s1,
                vec![Decl::VarDecl {
                    name: tc.x,
                    ty: None,
                    val: Expr::Literal {
                        val: Value::Int { val: 0 },
                    },
                }],
            )
            .await
            .unwrap();
        let older = crate::runtime::txn::TxnId {
            timestamp: 1,
            node_id: 1,
            iteration: 0,
        };
        tc.manager
            .services
            .get_mut(&tc.s1)
            .unwrap()
            .vars
            .get_mut(&tc.x)
            .unwrap()
            .lock = crate::runtime::txn::VarLock::WriteLocked(older);
        let stmts = vec![ActionStmt::Assign {
            name: tc.x,
            expr: Expr::Binop {
                op: crate::ast::BinOp::Add,
                expr1: Box::new(Expr::Variable { name: tc.x }),
                expr2: Box::new(Expr::Literal {
                    val: Value::Int { val: 1 },
                }),
            },
        }];
        let result = tc.manager.execute_action(tc.s1, &stmts).await;
        assert!(matches!(result, Err(EvalError::WaitDieAbort(_))));
        assert!(matches!(
            tc.manager
                .services
                .get(&tc.s1)
                .unwrap()
                .vars
                .get(&tc.x)
                .unwrap()
                .lock,
            crate::runtime::txn::VarLock::WriteLocked(_)
        ));
    }

    #[tokio::test]
    async fn test_wait_die_participant_preserves_partial_txn() {
        // Wait-die: a participant action that conflicts mid-execution
        // parks by preserving its partial transaction (locks already
        // taken stay held) in `pending_txns`, so a later re-dispatch
        // can resume rather than restart
        let mut tc = TestContext::new();
        tc.manager
            .create_service(
                tc.s1,
                vec![
                    Decl::VarDecl {
                        name: tc.y,
                        ty: None,
                        val: Expr::Literal {
                            val: Value::Int { val: 0 },
                        },
                    },
                    Decl::VarDecl {
                        name: tc.x,
                        ty: None,
                        val: Expr::Literal {
                            val: Value::Int { val: 0 },
                        },
                    },
                ],
            )
            .await
            .unwrap();
        // A younger transaction holds a write lock on `x`
        let younger = crate::runtime::txn::TxnId {
            timestamp: u128::MAX,
            node_id: 1,
            iteration: 0,
        };
        tc.manager
            .services
            .get_mut(&tc.s1)
            .unwrap()
            .vars
            .get_mut(&tc.x)
            .unwrap()
            .lock = crate::runtime::txn::VarLock::WriteLocked(younger);
        let older = crate::runtime::txn::TxnId {
            timestamp: 1,
            node_id: 1,
            iteration: 0,
        };
        // Older transaction: write `y` (acquires `y`), then touch `x`
        // (conflict, waits)
        let stmts = vec![
            ActionStmt::Assign {
                name: tc.y,
                expr: Expr::Literal {
                    val: Value::Int { val: 5 },
                },
            },
            ActionStmt::Assign {
                name: tc.x,
                expr: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Variable { name: tc.x }),
                    expr2: Box::new(Expr::Literal {
                        val: Value::Int { val: 1 },
                    }),
                },
            },
        ];
        let result = tc
            .manager
            .execute_action_participant(tc.s1, &stmts, &[], older.clone())
            .await;
        // Parked: returns `WaitOn`, and the partial transaction is preserved
        assert!(matches!(result, Err(EvalError::WaitOn(_, _))));
        assert!(tc.manager.pending_txns.contains_key(&older));
        // The lock it already took on `y` is still held (not released on park)
        assert!(matches!(
            tc.manager
                .services
                .get(&tc.s1)
                .unwrap()
                .vars
                .get(&tc.y)
                .unwrap()
                .lock,
            crate::runtime::txn::VarLock::WriteLocked(_)
        ));
    }

    #[tokio::test]
    async fn test_wait_queue_oldest_first_and_purge() {
        // Wait-die: parked requests on a variable are served oldest-first
        // when the lock frees, and a transaction's waiters are purged
        // when it aborts
        let mut tc = TestContext::new();
        tc.manager
            .create_service(
                tc.s1,
                vec![Decl::VarDecl {
                    name: tc.x,
                    ty: None,
                    val: Expr::Literal {
                        val: Value::Int { val: 0 },
                    },
                }],
            )
            .await
            .unwrap();
        let make = |rid: u64, tid: crate::runtime::txn::TxnId| ParkedRequest::Action {
            request_id: rid,
            reply_to: String::new(),
            service: tc.s1,
            stmts: vec![],
            env: vec![],
            tid,
        };
        let old = crate::runtime::txn::TxnId {
            timestamp: 1,
            node_id: 1,
            iteration: 0,
        };
        let mid = crate::runtime::txn::TxnId {
            timestamp: 5,
            node_id: 1,
            iteration: 0,
        };
        tc.manager.park_request(tc.s1, tc.x, make(1, mid.clone()));
        tc.manager.park_request(tc.s1, tc.x, make(2, old.clone()));
        // Freeing `x` yields the oldest waiter first; the other stays parked
        let mut freed = std::collections::HashSet::new();
        freed.insert((tc.manager.service_net_id_for_name(tc.s1), tc.x));
        let ready = tc.manager.take_ready_waiters(&freed);
        assert_eq!(ready.len(), 1);
        assert!(ready[0].tid() == &old);
        // The remaining `mid` waiter is purged when its transaction aborts
        let removed = tc.manager.purge_parked_txn(&mid);
        assert_eq!(removed.len(), 1);
        assert!(tc.manager.wait_queue.is_empty());
    }

    #[tokio::test]
    async fn test_wait_die_parked_request_resumes_after_release() {
        // Wait-die end to end (single node, no network): an older
        // transaction parks on a variable held by a younger one; when
        // the younger aborts and frees the lock, the parked request is
        // taken oldest-first and its re-run resumes from the preserved
        // transaction and now succeeds
        let mut tc = TestContext::new();
        tc.manager
            .create_service(
                tc.s1,
                vec![Decl::VarDecl {
                    name: tc.x,
                    ty: None,
                    val: Expr::Literal {
                        val: Value::Int { val: 0 },
                    },
                }],
            )
            .await
            .unwrap();

        // A younger transaction holds a write lock on `x`, prepared in
        // `pending_txns`
        let younger = crate::runtime::txn::TxnId {
            timestamp: u128::MAX,
            node_id: 1,
            iteration: 0,
        };
        tc.manager
            .services
            .get_mut(&tc.s1)
            .unwrap()
            .vars
            .get_mut(&tc.x)
            .unwrap()
            .lock = crate::runtime::txn::VarLock::WriteLocked(younger.clone());
        let mut younger_txn = crate::runtime::txn::Transaction::new(younger.clone());
        younger_txn
            .locked
            .insert((tc.manager.service_net_id_for_name(tc.s1), tc.x));
        tc.manager.pending_txns.insert(younger.clone(), younger_txn);

        let older = crate::runtime::txn::TxnId {
            timestamp: 1,
            node_id: 1,
            iteration: 0,
        };
        // Older transaction: `x` = `x` + 1 conflicts -> `WaitOn` -> park it
        let stmts = vec![ActionStmt::Assign {
            name: tc.x,
            expr: Expr::Binop {
                op: crate::ast::BinOp::Add,
                expr1: Box::new(Expr::Variable { name: tc.x }),
                expr2: Box::new(Expr::Literal {
                    val: Value::Int { val: 1 },
                }),
            },
        }];
        let r1 = tc
            .manager
            .execute_action_participant(tc.s1, &stmts, &[], older.clone())
            .await;
        assert!(matches!(r1, Err(EvalError::WaitOn(_, _))));
        tc.manager.park_request(
            tc.s1,
            tc.x,
            ParkedRequest::Action {
                request_id: 1,
                reply_to: String::new(),
                service: tc.s1,
                stmts: stmts.clone(),
                env: vec![],
                tid: older.clone(),
            },
        );

        // The younger holder aborts, freeing `x`
        let freed = tc.manager.abort_participant(&younger).await;
        assert!(matches!(
            tc.manager
                .services
                .get(&tc.s1)
                .unwrap()
                .vars
                .get(&tc.x)
                .unwrap()
                .lock,
            crate::runtime::txn::VarLock::Unlocked
        ));

        // Wake: take the oldest waiter and re-run it; it should now succeed
        let ready = tc.manager.take_ready_waiters(&freed);
        assert_eq!(ready.len(), 1);
        if let ParkedRequest::Action {
            service,
            stmts,
            env,
            tid,
            ..
        } = &ready[0]
        {
            let r2 = tc
                .manager
                .execute_action_participant(*service, stmts, env, tid.clone())
                .await;
            assert!(r2.is_ok());
        } else {
            panic!("expected an Action waiter");
        }

        // The older transaction now holds `x`'s write lock and is prepared
        assert!(matches!(
            tc.manager
                .services
                .get(&tc.s1)
                .unwrap()
                .vars
                .get(&tc.x)
                .unwrap()
                .lock,
            crate::runtime::txn::VarLock::WriteLocked(_)
        ));
        assert!(tc.manager.pending_txns.contains_key(&older));
    }
    #[tokio::test]
    async fn test_create_service_uses_single_transaction() {
        let mut tc = TestContext::new();
        let decls = vec![
            Decl::VarDecl {
                name: tc.x,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 1 },
                },
            },
            Decl::VarDecl {
                name: tc.y,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 2 },
                },
            },
            Decl::DefDecl {
                name: tc.f,
                ty: None,
                val: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Variable { name: tc.x }),
                    expr2: Box::new(Expr::Variable { name: tc.y }),
                },
                is_pub: true,
            },
        ];
        tc.manager.create_service(tc.foo, decls).await.unwrap();

        let foo = tc.manager.services.get(&tc.foo).unwrap();
        let tid = foo.vars.get(&tc.x).unwrap().latest_write_txn.clone();
        assert!(
            tid.is_some(),
            "init writes must record a writer txn from create_service"
        );
        // every var/def initialized by the same transaction
        assert_eq!(foo.vars.get(&tc.y).unwrap().latest_write_txn, tid);
        assert_eq!(foo.vars.get(&tc.f).unwrap().latest_write_txn, tid);
    }

    #[tokio::test]
    async fn test_create_service_rolls_back_on_partial_failure() {
        let mut tc = TestContext::new();
        let decls = vec![
            Decl::VarDecl {
                name: tc.x,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 1 },
                },
            },
            // adding a bool and number should be a type error
            Decl::VarDecl {
                name: tc.y,
                ty: None,
                val: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Literal {
                        val: Value::Bool { val: true },
                    }),
                    expr2: Box::new(Expr::Literal {
                        val: Value::Int { val: 1 },
                    }),
                },
            },
        ];
        let result = tc.manager.create_service(tc.foo, decls).await;
        assert!(result.is_err());
        assert!(
            tc.manager.services.is_empty(),
            "no services should've been created"
        );
    }

    // full disclosure: I'm not sure how to test that create_service actually occurs under a single transaction
    // in the sense that a lock is truly acquired, so I had Claude write a test for me
    #[tokio::test]
    async fn test_create_service_read_conflicts_under_one_txn() {
        let mut tc = TestContext::new();
        tc.manager
            .create_service(
                tc.s1,
                vec![Decl::VarDecl {
                    name: tc.x,
                    ty: None,
                    val: Expr::Literal {
                        val: Value::Int { val: 7 },
                    },
                }],
            )
            .await
            .unwrap();

        // Simulate another in-flight transaction holding a write lock on s1.x.
        let ext = TxnId::new(tc.manager.node_id);
        assert!(tc
            .manager
            .services
            .get_mut(&tc.s1)
            .unwrap()
            .vars
            .get_mut(&tc.x)
            .unwrap()
            .lock
            .try_write(&ext));

        // s2's init reads s1.x; under a real transaction this must fail to read-lock.
        // since s2 is younger, it should die instead of waiting
        let result = tc
            .manager
            .create_service(
                tc.s2,
                vec![Decl::DefDecl {
                    name: tc.f,
                    ty: None,
                    val: Expr::MemberAccess {
                        service_name: tc.s1,
                        member_name: tc.x,
                    },
                    is_pub: true,
                }],
            )
            .await;

        assert!(
            matches!(result, Err(EvalError::WaitDieAbort(_))),
            "init read must respect the lock"
        );
        assert!(
            !tc.manager.services.contains_key(&tc.s2),
            "failed init rolls back"
        );
        // the foreign lock is untouched
        assert!(matches!(
            tc.manager.services.get(&tc.s1).unwrap().vars.get(&tc.x).unwrap().lock,
            crate::runtime::txn::VarLock::WriteLocked(ref t) if *t == ext
        ));
    }
}
