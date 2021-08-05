///! Runner functions for participating in network
use crate::network::{
    accounts::Role,
    connections::{Connection, ConnectionPool},
    messages::{MessageData, NetworkMessage, ProcessMessage},
    nodes::Node,
};

use crate::blockchain::{Block, Data};

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::ops::DerefMut;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use anyhow::{Error, Result};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    Notify, RwLock,
};
use tokio::task;
use tracing::{debug, error, info};

/// Synchronisation struct to maintain the job queue
pub struct JobSync {
    nonce_set: RwLock<HashSet<u128>>,
    permits: AtomicUsize,
    notify: Notify,
    sender: Sender<ProcessMessage>,
    receiver: RwLock<Receiver<ProcessMessage>>,
}

impl JobSync {
    /// Creates a new instance of JobSync
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel::<ProcessMessage>(1000);
        Self {
            nonce_set: RwLock::new(HashSet::new()),
            permits: AtomicUsize::new(0),
            notify: Notify::new(),
            sender: tx,
            receiver: RwLock::new(rx),
        }
    }

    /// Adds a new permit to the synchronisation mechanism
    pub fn new_permit(&self) {
        debug!("Creating new permit");

        self.permits.fetch_add(1, Ordering::SeqCst);
        debug!("Notifing");
        self.notify.notify_one();
    }

    /// Claims a permit from the group of permits
    ///
    /// Claims a permit if one exists, if no permits exist, it will
    /// wait until a permit is available.
    pub async fn claim_permit(&self) {
        debug!("Claiming permit");
        let permits: usize = self.permits.load(Ordering::SeqCst);
        if permits == 0 {
            debug!("No available permits, Waiting for permit");
            self.notify.notified().await;
            debug!("Permit now available");
        }
        self.permits.fetch_sub(1, Ordering::SeqCst);
        debug!(
            "Claimed permit. Permits left: {}",
            self.permits.load(Ordering::SeqCst)
        );
    }
}

pub async fn run(role: Role) -> Result<()> {
    let node: Arc<Node> = Arc::new(Node::new(role));
    let connect_pool: Arc<ConnectionPool> = Arc::new(ConnectionPool::new());

    let sync: Arc<JobSync> = Arc::new(JobSync::new());

    let _inbound_fut = inbound(
        Arc::clone(&node),
        Arc::clone(&connect_pool),
        Arc::clone(&sync),
    );

    task::spawn(async move {
        outgoing_connections(
            Arc::clone(&node),
            Arc::clone(&connect_pool),
            Arc::clone(&sync),
        )
        .await
        .unwrap()
    })
    .await?;
    Ok(())
}

async fn inbound(
    node: Arc<Node>,
    connect_pool: Arc<ConnectionPool>,
    sync: Arc<JobSync>,
) -> Result<()> {
    // Thread to listen for inbound connections (reactive)
    //     Put all connections into connect pool
    let cp_copy = Arc::clone(&connect_pool);
    let node_copy = Arc::clone(&node);
    task::spawn(async move { incoming_connections(node_copy, cp_copy).await.unwrap() }).await?;
    // Thread to go through all connections and deal with incoming messages (reactive)
    let node_copy = Arc::clone(&node);
    let cp_copy = Arc::clone(&connect_pool);
    let sync_copy = Arc::clone(&sync);
    task::spawn(async move {
        check_connections(node_copy, cp_copy, sync_copy)
            .await
            .unwrap()
    })
    .await?;

    Ok(())
}

/// Uses [`TcpListener`] to allow incoming connections
///
/// Listens to a inbound port and accepts connections coming from other
/// participants in the network. Takes the connections and inserts them
/// into the `connect_pool`.
async fn incoming_connections(node: Arc<Node>, connect_pool: Arc<ConnectionPool>) -> Result<()> {
    #[cfg(not(debug_assertions))]
    let ip = Ipv4Addr::UNSPECIFIED;

    #[cfg(debug_assertions)]
    let ip = Ipv4Addr::LOCALHOST;

    // Temporary solution
    let port: u16 = 8080;

    // Open socket and start listening
    let socket = SocketAddr::V4(SocketAddrV4::new(ip, port));
    let listener = TcpListener::bind(&socket).await?;

    while let Ok((inbound, _)) = listener.accept().await {
        debug!("New Inbound Connection: {:?}", inbound.peer_addr());

        let cp_cp = Arc::clone(&connect_pool);
        let node_cp = Arc::clone(&node);

        let fut = process_connection(inbound, node_cp, cp_cp);

        if let Err(e) = tokio::spawn(async move { fut.await }).await? {
            error!("Error processing connection: {}", e);
        }
    }

    Ok(())
}

/// Takes in a [`TcpStream`] and adds it to the [`ConnectionPool`]
///
/// Function is given a [`TcpStream`]. It then gets the ID from the
/// stream so the [`Connection`] can be identified in the [`ConnectionPool`].
async fn process_connection(
    mut stream: TcpStream,
    node: Arc<Node>,
    connect_pool: Arc<ConnectionPool>,
) -> Result<()> {
    // Get the actual ID of the Connection from the stream
    let (id, pub_key, role) = match initial_stream_handler(&mut stream).await {
        Some((Data::NewUser { id, pub_key }, role)) => (id, pub_key, role),
        _ => return Err(Error::msg("Error getting initial data Message")),
    };

    debug!("ID of new connection: {}", id);

    // Send back initial ID
    let send_mess = NetworkMessage::new(MessageData::InitialID(
        Data::NewUser {
            id: node.account.id,
            pub_key: node.account.pub_key.clone(),
        },
        node.account.role,
    ));
    send_message(&mut stream, send_mess).await?;

    let conn = Connection::new(stream, role, Some(pub_key));
    connect_pool.add(conn, id).await;

    Ok(())
}

/// Finds the [`MessageData::InitialID`] in a [`TcpStream`]
async fn initial_stream_handler(stream: &mut TcpStream) -> Option<(Data, Role)> {
    let mut buffer = [0_u8; 4096];
    match NetworkMessage::from_stream(stream, &mut buffer).await {
        Ok(m) => match m.data {
            MessageData::InitialID(data, role) => Some((data, role)),
            _ => None,
        },
        _ => None,
    }
}

/// Goes through each [`Connection`] and checks to see if they contain a [`NetworkMessage`]
async fn check_connections(
    node: Arc<Node>,
    connect_pool: Arc<ConnectionPool>,
    sync: Arc<JobSync>,
) -> Result<()> {
    loop {
        debug!("Checking connections in ConnectionPool");

        let conns = connect_pool.map.read().await;

        for (_, conn) in conns.iter() {
            let stream_lock = conn.get_tcp();

            let node_cp = Arc::clone(&node);
            let sync_cp = Arc::clone(&sync);

            task::spawn(async move {
                let mut stream = stream_lock.write().await;
                let mut buffer = [0_u8; 4096];

                // Check to see if connection has a message
                let peeked = stream.peek(&mut buffer).await.unwrap();

                // If it does, pass to state machine
                if peeked > 0 {
                    let message: NetworkMessage =
                        NetworkMessage::from_stream(stream.deref_mut(), &mut buffer)
                            .await
                            .map_err(|_| Error::msg("Error with stream"))
                            .unwrap();

                    debug!("New Message: {:?}", &message);

                    recv_state_machine(node_cp, sync_cp, message).await.unwrap();
                }
                // if not, move on
            });
        }
    }
}

/// Deals with incoming messages from each [`Connection`] in the [`ConnectionPool`]
///
/// Listens to each [`Connection`] and consumes any messages from the associated [`TcpStream`].
/// This message is then dealt with. Each [`NetworkMessage`] is processed using a state machine
/// structure, which is best suited to the unpredictable nature of the incoming messages.
async fn recv_state_machine(
    node: Arc<Node>,
    sync: Arc<JobSync>,
    message: NetworkMessage,
) -> Result<()> {
    match &message.data {
        MessageData::Event(e) => {
            // If miner, add it to list of events to build Block
            let pm = match node.account.role {
                Role::Miner => {
                    debug!("Recv Event: {:?}", e);
                    let mut unlocked_events = node.loose_events.write().await;
                    let mut bc_unlocked = node.blockchain.write().await;
                    let mut ns_unlocked = sync.nonce_set.write().await;
                    // Check if event is already in loose_events and not in blockchain
                    if !ns_unlocked.contains(&e.nonce)
                        && !unlocked_events.contains(&e)
                        && !bc_unlocked.contains(&e)
                    {
                        debug!("Event is new");
                        // If it is not, add to set
                        unlocked_events.push(e.clone());
                        // if Vec over threshold size, build block and empty loose_events
                        // TODO: Abstract out threshold size
                        if unlocked_events.len() > 100 {
                            debug!("Building new block");
                            let last_hash = bc_unlocked.last_hash();
                            let mut block: Block = Block::new(last_hash);
                            block.add_events(unlocked_events.clone());
                            bc_unlocked.append(block.clone())?;
                            *unlocked_events = Vec::new();
                            ns_unlocked.insert(e.nonce);
                            ns_unlocked.insert(block.nonce);
                            Some(ProcessMessage::SendMessage(NetworkMessage::new(
                                MessageData::Block(block),
                            )))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                _ => {
                    // Else, pass onto other connections
                    let mut ns_unlocked = sync.nonce_set.write().await;
                    if !ns_unlocked.contains(&e.nonce) {
                        ns_unlocked.insert(e.nonce);
                        Some(ProcessMessage::SendMessage(message.clone()))
                    } else {
                        None
                    }
                }
            };

            if let Some(m) = pm {
                match sync.sender.send(m).await {
                    Ok(_) => {
                        debug!("Added new ProcessMessage to Pipe");
                        sync.new_permit();
                        Ok(())
                    }
                    _ => Err(Error::msg("Error writing to pipeline")),
                }
            } else {
                Ok(())
            }
        }
        MessageData::Block(b) => {
            // Check if alreday in Blockchain
            // If not in blockchain (and is valid),
            if !node.in_chain(&b).await {
                // add to blockchain
                node.add_block(b.clone()).await?;
                // pass onto other connected nodes
                let message: NetworkMessage = NetworkMessage::new(MessageData::Block(b.clone()));
                let process_message: ProcessMessage = ProcessMessage::SendMessage(message);
                match sync.sender.send(process_message).await {
                    Ok(_) => {
                        debug!("Added new ProcessMessage to Pipe");
                        sync.new_permit();
                        Ok(())
                    }
                    _ => Err(Error::msg("Error writing to pipeline")),
                }?;
            }
            // Ignore if already in blockchain

            Ok(())
        }
        MessageData::State(bc) => {
            debug!("New blockchain received");
            // Check if valid
            if bc.validate_chain().is_ok() {
                debug!("New blockchain is valid");
                // If valid, check if it is a subchain of current blockchain
                if bc.len() > node.bc_len().await && node.chain_overlap(&bc).await > 0.5 {
                    // If longer and contains more than half of original chain, replace
                    info!("New blockchain received, old Blockchain replaced");
                    let mut bc_unlocked = node.blockchain.write().await;
                    *bc_unlocked = bc.clone();
                }
                // If shorter, ignore
            }
            // If not valid, ignore
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Forges new outgoing connections and adds them to the [`ConnectionPool`].
///
/// Consumes data from pipeline which instructs it to perform certain actions. This could be to
/// try and create a connection with another member of the network via a [`TcpStream`].
async fn outgoing_connections(
    node: Arc<Node>,
    connect_pool: Arc<ConnectionPool>,
    sync: Arc<JobSync>,
) -> Result<()> {
    let mut unsent_q: Vec<ProcessMessage> = Vec::new();
    loop {
        // Check if there are any jobs in unsent_q
        if unsent_q.len() > 0 {
            debug!("Getting ProcessMessage from Queue");

            if let Some((i, ProcessMessage::NewConnection(addr))) = unsent_q
                .iter()
                .enumerate()
                .filter(|(_, m)| matches!(m, ProcessMessage::NewConnection(_)))
                .take(1)
                .next()
            {
                debug!("Forming connection with: {}", addr);

                create_connection(
                    Arc::clone(&node),
                    String::from(addr),
                    Arc::clone(&connect_pool),
                )
                .await?;

                debug!("Removing element: {}", i);

                unsent_q.remove(i);
            }
        } else {
            // Wait until there is something in the pipeline
            sync.claim_permit().await;

            let num_conns: usize = async {
                let unlocked_map = connect_pool.map.read().await;
                unlocked_map.len()
            }
            .await;

            // Read pipeline for new messages
            let mut rx = sync.receiver.write().await;
            // When new message comes through pipeline
            if let Some(m) = rx.recv().await {
                debug!("Received message from pipeline: {:?}", &m);
                // Take message
                // Process message by reading using match to determine what to do
                // take action based on message

                match &m {
                    ProcessMessage::SendMessage(net_mess) => {
                        if num_conns == 0 {
                            debug!("No connections, so adding to unsent queue");
                            unsent_q.push(m.clone());
                            continue;
                        }
                        send_all(net_mess.clone(), Arc::clone(&connect_pool)).await
                    }
                    ProcessMessage::NewConnection(addr) => {
                        create_connection(
                            Arc::clone(&node),
                            String::from(addr),
                            Arc::clone(&connect_pool),
                        )
                        .await
                    }
                    _ => unreachable!(),
                }?
            }
        }
    }
}

/// Sends a new message to a [`Connection`] in [`ConnectionPool`]
///
/// Takes in a new [`NetworkMessage`] and distributes it to a [`Connection`] in the
/// [`ConnectionPool`] so they are aware of the information which is bein spread.
async fn send_message(stream: &mut TcpStream, message: NetworkMessage) -> Result<()> {
    debug!("Sending Message: {:?}", &message);
    let bytes_message = message.as_bytes();
    stream.write_all(&bytes_message).await?;
    Ok(())
}

/// Takes in [`NetworkMessage`] and sends it to all intended recipients
///
/// Gets a [`NetworkMessage`] and either floods all connections with the message
/// that is being sent, or will send it to all connected nodes
async fn send_all(message: NetworkMessage, connect_pool: Arc<ConnectionPool>) -> Result<()> {
    debug!("Sending message to all connected participants");
    let conn_map = connect_pool.map.read().await;
    for (_, conn) in conn_map.iter() {
        let tcp = conn.get_tcp();
        let mut stream = tcp.write().await;

        debug!("Sending message to: {:?}", stream.peer_addr());

        send_message(stream.deref_mut(), message.clone()).await?;
    }
    Ok(())
}

/// Given an address and port, creates connection with new node
///
/// Function is passed an address and a port and it will attempt to
/// create a TCP connection with the node at that address
async fn create_connection(
    node: Arc<Node>,
    address: String,
    connect_pool: Arc<ConnectionPool>,
) -> Result<()> {
    debug!("Creating Connection with: {}", address);
    // Open connection
    let mut stream: TcpStream = TcpStream::connect(address).await?;

    // Send initial message with ID
    let send_mess = NetworkMessage::new(MessageData::InitialID(
        Data::NewUser {
            id: node.account.id,
            pub_key: node.account.pub_key.clone(),
        },
        node.account.role,
    ));
    send_message(&mut stream, send_mess).await?;

    // Recv initial message with ID
    let (id, pub_key, role) = match initial_stream_handler(&mut stream).await {
        Some((Data::NewUser { id, pub_key }, role)) => (id, pub_key, role),
        _ => return Err(Error::msg("Error getting initial data Message")),
    };

    // Add to map
    connect_pool
        .add(Connection::new(stream, role, Some(pub_key)), id)
        .await;
    Ok(())
}
