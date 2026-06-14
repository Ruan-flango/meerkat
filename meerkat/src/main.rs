mod repl;

use clap::Parser;
use meerkat_lib::net::network_layer::NetworkLayer;
use meerkat_lib::net::types::NodeType;
use meerkat_lib::net::NetworkActor;
use meerkat_lib::net::{Address, MeerkatMessage, NetworkCommand, NetworkEvent};
use meerkat_lib::runtime::ast::Stmt;
use meerkat_lib::runtime::Manager;
use std::error::Error;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// Input file to run. Omit to launch the interactive REPL.
    #[arg(short = 'f', long = "file")]
    input_file: Option<String>,

    #[arg(short = 'v', long = "verbose", default_value_t = false)]
    verbose: bool,

    /// Server mode: start a server providing the services in the input file
    #[arg(short = 's', long = "server", default_value_t = false)]
    server: bool,

    /// Remote service URLs: -i <url> maps the service slug to a remote address
    #[arg(short = 'i', long = "import-url")]
    import_urls: Vec<String>,

    /// Port to listen on in server mode (default: 9000)
    #[arg(short = 'p', long = "port", default_value_t = 9000)]
    port: u16,

    /// Bind to loopback/localhost only (force 127.0.0.1 instead of public IP)
    #[arg(long = "local", default_value_t = false)]
    local: bool,
}

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();

    let log_level = if args.verbose {
        log::LevelFilter::Info
    } else {
        log::LevelFilter::Warn
    };
    env_logger::Builder::from_default_env()
        .filter_level(log_level)
        .init();

    // Build slug -> remote address map from -i flags
    let mut remote_url_map: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for url in &args.import_urls {
        if let Some(slug) = url.split('/').last() {
            remote_url_map.insert(slug.to_string(), url.clone());
        }
    }

    match args.input_file {
        Some(ref file) => {
            let prog = meerkat_lib::runtime::parser::parser::parse_file(file)
                .map_err(|e| format!("Parse error: {}", e))?;

            if args.server {
                run_server(prog, remote_url_map, args.port, args.local).await
            } else {
                run_client(prog, file, remote_url_map, args.local).await
            }
        }
        None => {
            if args.server {
                return Err("-s/--server requires a file (-f). Pass a .mkt file containing the services to host.".into());
            }
            let mut manager = Manager::new();
            manager.local = args.local;
            repl::run_repl(manager, remote_url_map).await
        }
    }
}

async fn run_server(
    prog: Vec<Stmt>,
    remote_url_map: std::collections::HashMap<String, String>,
    port: u16,
    local: bool,
) -> Result<(), Box<dyn Error>> {
    let mut net = NetworkActor::new(NodeType::Server).await?;
    let mut manager = Manager::new();
    manager.local = local;

    let node_ip = manager.get_node_ip();
    let listen_ip = if local { "127.0.0.1" } else { "0.0.0.0" };
    let listen_addr = Address::new(&format!("/ip4/{}/tcp/{}", listen_ip, port));
    let reply = net
        .handle_command(NetworkCommand::Listen { addr: listen_addr })
        .await;
    let actual_addr = match reply {
        meerkat_lib::net::NetworkReply::ListenSuccess { addr } => addr,
        meerkat_lib::net::NetworkReply::Failure(e) => return Err(e.into()),
        _ => return Err("Unexpected reply".into()),
    };

    let peer_id = net.local_peer_id();
    // Replace loopback/unspecified with actual node IP
    let actual_addr_str = actual_addr
        .0
        .replace("0.0.0.0", &node_ip)
        .replace("127.0.0.1", &node_ip);
    let full_addr = format!("{}/p2p/{}", actual_addr_str, peer_id);
    println!("Server listening at: {}", full_addr);

    // Print service URLs
    for stmt in &prog {
        if let Stmt::Service { name, .. } = stmt {
            println!("Service URL: {}/{}", full_addr, name);
        }
    }

    // Register any remote services from -i flags
    for (svc_name, url) in &remote_url_map {
        manager
            .remote_services
            .insert(svc_name.clone(), Address::new(url.as_str()));
        println!("Remote service '{}' registered at {}", svc_name, url);
    }

    // Wire network into manager so server can also do remote lookups
    manager.network = Some(net);
    // Record the canonical address so service identities are stable and match
    // the advertised Service URLs above.
    manager.set_local_address(full_addr.clone());

    // Load services after network and remote services are ready,
    // so that remote lookups during service initialization work correctly
    for stmt in &prog {
        if let Stmt::Service { name, decls } = stmt {
            manager
                .create_service(name.clone(), decls.clone())
                .await
                .map_err(|e| format!("Service error: {}", e))?;
            println!("Service '{}' loaded", name);
        }
    }

    println!("Server running, press Ctrl+C to stop...");

    loop {
        let event = manager.network.as_mut().and_then(|n| n.try_recv_event());
        if let Some(event) = event {
            match event {
                NetworkEvent::MessageReceived { peer: _, msg } => {
                    match msg {
                        MeerkatMessage::LookupRequest {
                            request_id,
                            service,
                            member,
                            reply_to,
                            txn_id,
                        } => {
                            // Transactional read: acquire and hold a read lock
                            // under the shared id. Plain read otherwise.
                            let result = match txn_id {
                                Some(tid) => {
                                    manager
                                        .remote_read_participant(&service, &member, tid)
                                        .await
                                }
                                None => manager.lookup(&member, &service, None).await,
                            };
                            let response = match result {
                                Ok(val) => MeerkatMessage::LookupResponse {
                                    request_id,
                                    value: serde_json::to_string(&val).unwrap_or_default(),
                                },
                                Err(e) => MeerkatMessage::LookupError {
                                    request_id,
                                    error: e.to_string(),
                                },
                            };
                            if let Some(net) = manager.network.as_mut() {
                                net.handle_command(NetworkCommand::SendMessage {
                                    addr: Address::new(&reply_to),
                                    msg: response,
                                })
                                .await;
                            }
                        }
                        MeerkatMessage::ActionRequest {
                            request_id,
                            service,
                            stmts,
                            env: action_env,
                            reply_to,
                            txn_id,
                        } => {
                            // Part of a distributed transaction: execute under the
                            // shared id and hold. Standalone: commit immediately.
                            let result = match txn_id {
                                Some(tid) => {
                                    manager
                                        .execute_action_participant(
                                            &service,
                                            &stmts,
                                            &action_env,
                                            tid,
                                        )
                                        .await
                                }
                                None => {
                                    manager
                                        .execute_action_with_env(&service, &stmts, &action_env)
                                        .await
                                }
                            };
                            let response = MeerkatMessage::ActionResponse {
                                request_id,
                                success: result.is_ok(),
                                error: result.err().map(|e| e.to_string()),
                            };
                            if let Some(net) = manager.network.as_mut() {
                                net.handle_command(NetworkCommand::SendMessage {
                                    addr: Address::new(&reply_to),
                                    msg: response,
                                })
                                .await;
                            }
                        }
                        MeerkatMessage::Commit {
                            request_id,
                            txn_id,
                            reply_to,
                        } => {
                            let result = manager.commit_participant(&txn_id).await;
                            let response = MeerkatMessage::CommitResponse {
                                request_id,
                                success: result.is_ok(),
                                error: result.err().map(|e| e.to_string()),
                            };
                            if let Some(net) = manager.network.as_mut() {
                                net.handle_command(NetworkCommand::SendMessage {
                                    addr: Address::new(&reply_to),
                                    msg: response,
                                })
                                .await;
                            }
                        }
                        MeerkatMessage::Abort {
                            request_id,
                            txn_id,
                            reply_to,
                        } => {
                            manager.abort_participant(&txn_id).await;
                            if let Some(net) = manager.network.as_mut() {
                                net.handle_command(NetworkCommand::SendMessage {
                                    addr: Address::new(&reply_to),
                                    msg: MeerkatMessage::AbortResponse { request_id },
                                })
                                .await;
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
}

async fn run_client(
    prog: Vec<Stmt>,
    input_file: &str,
    remote_url_map: std::collections::HashMap<String, String>,
    local: bool,
) -> Result<(), Box<dyn Error>> {
    let mut manager = Manager::new();
    manager.local = local;

    // Start network if we have remote imports
    let mut net: Option<NetworkActor> = None;
    let mut local_full_addr: Option<String> = None;
    if !remote_url_map.is_empty() {
        let mut n = NetworkActor::new(NodeType::Server)
            .await
            .map_err(|e| format!("Network error: {}", e))?;
        let listen_ip = if local { "127.0.0.1" } else { "0.0.0.0" };
        let listen_addr = Address::new(&format!("/ip4/{}/tcp/0", listen_ip));
        let reply = n
            .handle_command(NetworkCommand::Listen { addr: listen_addr })
            .await;
        if let meerkat_lib::net::NetworkReply::ListenSuccess { addr } = reply {
            let node_ip = manager.get_node_ip();
            let peer_id = n.local_peer_id();
            let addr_str = addr
                .0
                .replace("0.0.0.0", &node_ip)
                .replace("127.0.0.1", &node_ip);
            local_full_addr = Some(format!("{}/p2p/{}", addr_str, peer_id));
        }
        net = Some(n);
    }

    // Wire network actor into manager
    if let Some(n) = net {
        manager.network = Some(n);
    }
    // Record the canonical address (if networked) so service identities are
    // stable for the life of the process.
    if let Some(addr) = local_full_addr {
        manager.set_local_address(addr);
    }

    for stmt in &prog {
        match stmt {
            Stmt::Service { name, decls } => {
                manager
                    .create_service(name.clone(), decls.clone())
                    .await
                    .map_err(|e| format!("Service error: {}", e))?;
                println!("Service '{}' loaded", name);
            }
            Stmt::Test { service, stmts } => {
                manager
                    .execute_action(service, stmts)
                    .await
                    .map_err(|e| format!("Test failed in '{}': {}", service, e))?;
                println!("@test({}) passed", service);
            }
            Stmt::Import {
                path,
                service: svc_name,
            } => {
                if let Some(url) = remote_url_map.get(svc_name) {
                    manager
                        .remote_services
                        .insert(svc_name.clone(), Address::new(url.as_str()));
                    println!("Remote service '{}' registered at {}", svc_name, url);
                } else {
                    let base_dir = std::path::Path::new(input_file)
                        .parent()
                        .unwrap_or(std::path::Path::new("."));
                    let import_path = base_dir.join(path);
                    let import_stmts = meerkat_lib::runtime::parser::parser::parse_file(
                        import_path.to_str().unwrap(),
                    )
                    .map_err(|e| format!("Import parse error: {}", e))?;
                    for import_stmt in &import_stmts {
                        if let Stmt::Service { name, decls } = import_stmt {
                            manager
                                .create_service(name.clone(), decls.clone())
                                .await
                                .map_err(|e| format!("Import service error: {}", e))?;
                            println!("Imported service '{}'", name);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Ok(())
}
