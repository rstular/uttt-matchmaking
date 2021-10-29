#[macro_use]
extern crate lazy_static;
mod config;

use env_logger::Env;
use log::{debug, error, info, trace, warn};
use rand::Rng;

use mio::event::Event;
use mio::net::{TcpKeepalive, TcpListener, TcpSocket, TcpStream};
use mio::{Events, Interest, Poll, Registry, Token};

use std::cell::{RefCell, RefMut};
use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::result::Result;
use std::time::Duration;

const SERVER: Token = Token(0);

fn main() -> Result<(), Box<dyn Error>> {
    // Initialize the logger
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();
    info!("Starting UTTT matchmaking server");

    // Create a socket
    let socket: TcpSocket = match TcpSocket::new_v4() {
        Ok(s) => {
            trace!("Successfully created a socket");
            s
        }
        Err(error) => {
            error!("Could not create an IPv4 socket: {}", error.to_string());
            return Err(Box::new(error));
        }
    };

    // Set keep-alive settings (from configuration file)
    let keepalive: TcpKeepalive = TcpKeepalive::default()
        .with_time(Duration::from_secs(config::CONFIG_FILE.keepalive_interval))
        .with_interval(Duration::from_secs(config::CONFIG_FILE.keepalive_interval))
        .with_retries(config::CONFIG_FILE.keepalive_retries);
    match socket.set_keepalive_params(keepalive) {
        Ok(_) => {
            trace!(
                "Successfully set KeepAlive parameters (time = interval = {}, retries = {})",
                config::CONFIG_FILE.keepalive_interval,
                config::CONFIG_FILE.keepalive_retries
            );
        }
        Err(error) => {
            warn!("Could not set KeepAlive parameters: {}", error.to_string());
        }
    };

    // Parse IP address
    let bind_ip: Ipv4Addr = match config::CONFIG_FILE.listen_addr.parse() {
        Ok(addr) => addr,
        Err(error) => {
            error!("Could not parse provided IP address: {}", error.to_string());
            return Err(Box::new(error));
        }
    };
    // Combine parsed IP and port
    let bind_addr: SocketAddr =
        SocketAddr::new(IpAddr::V4(bind_ip), config::CONFIG_FILE.listen_port);
    // Create TCP listener and bind it to IP:port
    let mut server: TcpListener = match TcpListener::bind(bind_addr) {
        Ok(s) => {
            trace!(
                "Successfully bound to '{}:{}'",
                config::CONFIG_FILE.listen_addr,
                config::CONFIG_FILE.listen_port
            );
            s
        }
        Err(error) => {
            error!(
                "Could not bind on '{}:{}': {}",
                config::CONFIG_FILE.listen_addr,
                config::CONFIG_FILE.listen_port,
                error.to_string()
            );
            return Err(Box::new(error));
        }
    };

    // Create Poll object (will take care of polling for events)
    let mut poll: Poll = match Poll::new() {
        Ok(p) => p,
        Err(error) => {
            error!("Could not create a Poll object: {}", error.to_string());
            return Err(Box::new(error));
        }
    };
    // Initialize event store (we have capacity for 128 events in one cycle)
    let mut events = Events::with_capacity(128);

    // Register server with the poll (the poll object will handle events for our server)
    match poll
        .registry()
        .register(&mut server, SERVER, Interest::READABLE)
    {
        Ok(_) => {
            trace!("Successfully registered the server with the poll");
        }
        Err(error) => {
            error!(
                "Could not register server with the poll: {}",
                error.to_string()
            );
        }
    };

    // Initialize RNG
    let mut rng = rand::thread_rng();

    // Initialize token list - each TCP connection has its own unique token
    let mut unique_token = Token(SERVER.0 + 1);
    // Collection which maps token to connection entries (TcpListener and associated connection)
    let mut connections: HashMap<Token, RefCell<ConnectionEntry>> = HashMap::new();
    // Collection of tokens from players, who are looking for a match
    let mut unpaired_connections: VecDeque<Token> = VecDeque::new();

    info!(
        "Ready to receive connections on {}!",
        server.local_addr().unwrap()
    );

    // We wanna execute the polling forever
    loop {
        // Poll for events
        match poll.poll(&mut events, None) {
            Ok(_) => (),
            Err(error) => {
                warn!("Error when polling for events: {}", error.to_string());
            }
        };

        // Iterate through raised events
        for event in events.iter() {
            // We base our actions on the token of the event
            match event.token() {
                // It's a server event, meaning that something is happening with the state of a connection
                SERVER => loop {
                    // Accept all incoming connections - get TcpStream object and associated address
                    let (mut connection, address) = match server.accept() {
                        Ok((connection, address)) => (connection, address),
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            // We handled all connections
                            break;
                        }
                        // If we cannot accept a connection, something is really wrong
                        Err(error) => {
                            error!(
                                "An error occured while accepting a connection: {}",
                                error.to_string()
                            );
                            return Err(Box::new(error));
                        }
                    };

                    info!("Accepted connection from: {}", address);
                    // Generate a token for the accepted client
                    let token = next(&mut unique_token);
                    // Register the client with the poller - we will be polling for client events from now on
                    poll.registry().register(
                        &mut connection,
                        token,
                        Interest::READABLE.add(Interest::WRITABLE),
                    )?;

                    // Construct HashMap entry - currently, the connection is not associated yet
                    let mut conn_entry: ConnectionEntry = ConnectionEntry {
                        connection,
                        assoc_token: None,
                    };

                    // Check if there are any unpaired connections
                    if unpaired_connections.is_empty() {
                        // If not, this is the first client
                        debug!("No unpaired connections, adding this connection to unpaired list");
                        unpaired_connections.push_back(token);
                    } else {
                        // If there exist unpaired connections, attempt pairing
                        debug!(
                            "Unpaired connections available ({}), finding a match...",
                            unpaired_connections.len()
                        );

                        let mut assoc_conn_tkn;
                        let mut assoc_conn_entry_ref;

                        // We will loop through all unpaired connections until we find an appropriate candidate
                        loop {
                            // Get the token of an unpaired connection. This list *should* not be empty, but
                            // we can recover in the case it is
                            assoc_conn_tkn = match unpaired_connections.pop_front() {
                                Some(tkn) => {
                                    trace!("Attempting to pair with {:?}", tkn);
                                    tkn
                                }
                                None => {
                                    warn!("Unpaired connection list is unexpectedly empty");
                                    token
                                }
                            };
                            // Fetch the ConnectionEntry of the connection identified by the token
                            assoc_conn_entry_ref = connections.get_mut(&assoc_conn_tkn);
                            // If the connection entry does not exist (the client has disappeared),
                            // continue the search. If we've exhausted the list of unpaired connections,
                            // abort the search.
                            if assoc_conn_entry_ref.is_some() || unpaired_connections.is_empty() {
                                trace!("Search for unpaired connection ended");
                                break;
                            }
                        }
                        // If we weren't able to find an appropriate unpaired connection, add the current
                        // client to the list of unpaired connections
                        if let Some(assoc_conn_entry_unwr) = assoc_conn_entry_ref {
                            // We were able to find an appropriate unpaired connection. We will attempt pairing.
                            trace!(
                                "Matched an unpaired connection [{:?}]. Pairing...",
                                assoc_conn_tkn
                            );
                            // Set the "associated token" field of the current connection
                            conn_entry.assoc_token = Some(assoc_conn_tkn);
                            // Get the mutable ConnectionEntry of the associated connection
                            let mut assoc_conn_entry = assoc_conn_entry_unwr.borrow_mut();
                            // Set the "associated token" field
                            assoc_conn_entry.assoc_token = Some(token);

                            // We've successfully paired the connections. Now, we need to notify the clients
                            // and tell them who is to begin with the game
                            let starting_player: u8 = (rng.gen::<u8>() & 1) + 1;
                            trace!("Broadcasting starting player (player1: {:?} | player2: {:?} | begins: {})",
                                token, assoc_conn_tkn, starting_player);
                            // Construct the packet - packet type (3) + starting player (1 | 2)
                            let mut starting_player_packet: [u8; 2] = [3, starting_player];
                            // Notify player 1
                            match conn_entry.connection.write(&starting_player_packet) {
                                Ok(_) => {
                                    trace!(
                                        "Successfully notified client [{:?}] ({})",
                                        token,
                                        address
                                    );
                                }
                                Err(error) => {
                                    warn!(
                                        "Could not notify client [{:?}] ({}): {}",
                                        token,
                                        address,
                                        error.to_string()
                                    );
                                }
                            };
                            // Flip bit 1 and bit 2 and notify the associated player (player 2)
                            starting_player_packet = [3, starting_player ^ 3];
                            match assoc_conn_entry.connection.write(&starting_player_packet) {
                                Ok(_) => {
                                    debug!(
                                        "Successfully notified associated client [{:?}] ({})",
                                        assoc_conn_tkn,
                                        assoc_conn_entry.connection.peer_addr().unwrap_or_else(
                                            |_| SocketAddr::new(
                                                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                                                0
                                            )
                                        )
                                    );
                                }
                                Err(error) => {
                                    warn!(
                                        "Could not notify associated client [{:?}] ({}): {}",
                                        assoc_conn_tkn,
                                        assoc_conn_entry.connection.peer_addr().unwrap_or_else(
                                            |_| SocketAddr::new(
                                                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                                                0
                                            )
                                        ),
                                        error.to_string()
                                    );
                                }
                            };
                            info!(
                                "Finished pairing clients [addr='{}' {:?}] and [addr='{}' {:?}]",
                                address,
                                token,
                                assoc_conn_entry.connection.peer_addr().unwrap_or_else(|_| {
                                    SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
                                }),
                                assoc_conn_tkn
                            );
                        } else {
                            unpaired_connections.push_back(token);
                            trace!("Unpaired connection no longer available, adding connection to the queue");
                        }
                    }

                    // Add the connection entry to the HashMap
                    connections.insert(token, RefCell::new(conn_entry));
                },
                // It's a client-related event - token indicates the client
                token => {
                    // Done indicates whether the connection is over
                    // First, we get the ConnectionEntry associated with the token
                    let done = if let Some(connection) = connections.get(&token) {
                        // We load the ConnectionEntry of the associated connection
                        let assoc_conn_obj = match connection.borrow().assoc_token {
                            Some(tkn) => connections
                                .get(&tkn)
                                .map(|conn_info| conn_info.borrow_mut()),
                            None => None,
                        };
                        // We call the handler function. If the function tells us the connection
                        // is over, of it errors, we drop the connection.
                        match handle_connection_event(
                            poll.registry(),
                            &mut connection.borrow_mut(),
                            assoc_conn_obj,
                            event,
                        ) {
                            Ok(x) => x,
                            Err(error) => {
                                error!(
                                    "An error occured while handling connection event: {} [{:?}]",
                                    error.to_string(),
                                    token
                                );
                                true
                            }
                        }
                    } else {
                        false
                    };
                    if done {
                        trace!(
                            "Removing connection from internal collections [{:?}]",
                            token
                        );
                        // If the associated connection has a peer, we need to
                        // add the pear to the list of unpaired connections
                        if let Some(assoc_token) =
                            connections.get(&token).unwrap().borrow().assoc_token
                        {
                            unpaired_connections.push_back(assoc_token);
                        }
                        // Remove the connection entry
                        connections.remove(&token);
                    }
                }
            }
        }
    }
}

// Update the global token object, and return a unique token
fn next(current: &mut Token) -> Token {
    let next = current.0;
    current.0 += 1;
    Token(next)
}

// ConnectionEntry structure
struct ConnectionEntry {
    connection: TcpStream,
    assoc_token: Option<Token>,
}

// Connection handler function
fn handle_connection_event(
    registry: &Registry,
    conn: &mut ConnectionEntry,
    assoc_conn: Option<RefMut<ConnectionEntry>>,
    event: &Event,
) -> std::io::Result<bool> {
    if event.is_writable() {
        // We can (maybe) write to the connection.
        // Send an empty packet. In my defense, I was tired AF when I wrote this shit.
        let data_len: usize = 0;
        match conn.connection.write(b"") {
            // We want to write the entire `DATA` buffer in a single go. If we
            // write less we'll return a short write error (same as
            // `io::Write::write_all` does).
            Ok(n) if n < data_len => return Err(ErrorKind::WriteZero.into()),
            Ok(_) => {
                trace!(
                    "Successfully written data to connection ({})",
                    conn.connection
                        .peer_addr()
                        .unwrap_or_else(|_| SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
                );
                // After we've written something we'll reregister the connection
                // to only respond to readable events.
                registry.reregister(&mut conn.connection, event.token(), Interest::READABLE)?
            }
            // Would block "errors" are the OS's way of saying that the
            // connection is not actually ready to perform this I/O operation.
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                trace!(
                    "Connection not ready for writing ({})",
                    conn.connection
                        .peer_addr()
                        .unwrap_or_else(|_| SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
                );
            }
            // Got interrupted (how rude!), we'll try again.
            Err(error) if error.kind() == ErrorKind::Interrupted => {
                trace!(
                    "Interrupted while writing to connection ({})",
                    conn.connection
                        .peer_addr()
                        .unwrap_or_else(|_| SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
                );
                return handle_connection_event(registry, conn, None, event);
            }
            // Other errors we'll consider fatal.
            Err(err) => return Err(err),
        }
    }

    if event.is_readable() {
        let mut connection_closed = false;
        let mut received_data: [u8; 2] = [0; 2];
        let mut bytes_read = 0;

        loop {
            // Start reading
            match conn.connection.read(&mut received_data[bytes_read..]) {
                // If we read 0 bytes, the connection is closed
                Ok(0) => {
                    connection_closed = true;
                    break;
                }
                // If we read a non-zero number of bytes, we add the bytes to our buffer
                Ok(n) => {
                    trace!(
                        "Read {} bytes ({})",
                        n,
                        conn.connection
                            .peer_addr()
                            .unwrap_or_else(|_| SocketAddr::new(
                                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                                0
                            ))
                    );
                    // If we filled the buffer, stop reading
                    bytes_read += n;
                    if bytes_read >= received_data.len() {
                        break;
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    trace!(
                        "WouldBlock while reading data ({})",
                        conn.connection
                            .peer_addr()
                            .unwrap_or_else(|_| SocketAddr::new(
                                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                                0
                            ))
                    );
                    break;
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {
                    trace!(
                        "Interrupted while reading data ({})",
                        conn.connection
                            .peer_addr()
                            .unwrap_or_else(|_| SocketAddr::new(
                                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                                0
                            ))
                    );
                    break;
                }
                Err(error) => {
                    error!(
                        "An error occured while reading data: {} ({})",
                        error.to_string(),
                        conn.connection
                            .peer_addr()
                            .unwrap_or_else(|_| SocketAddr::new(
                                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                                0
                            ))
                    );
                    return Err(error);
                }
            };
        }

        // Check whether we read enough bytes
        if bytes_read == 2 {
            // Check if the connection has an associated connection
            if let Some(mut assoc_conn_extr) = assoc_conn {
                // If so, send the bytes from us to them
                match (*assoc_conn_extr).connection.write(&received_data) {
                    Ok(_) => {
                        trace!(
                            "Successfully written to associated stream ({})",
                            (*assoc_conn_extr)
                                .connection
                                .peer_addr()
                                .unwrap_or_else(|_| SocketAddr::new(
                                    IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                                    0
                                ))
                        );
                    }
                    Err(error) => {
                        warn!(
                            "Could not write to associated stream: {} ({})",
                            error.to_string(),
                            (*assoc_conn_extr)
                                .connection
                                .peer_addr()
                                .unwrap_or_else(|_| SocketAddr::new(
                                    IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                                    0
                                ))
                        );
                    }
                };
            } else {
                trace!("Connection has no associated peer");
            }
        }

        // End the handler if the connection is closed
        if connection_closed {
            debug!(
                "Connection closed by {}",
                conn.connection
                    .peer_addr()
                    .unwrap_or_else(|_| SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
            );
            return Ok(true);
        }
    }

    Ok(false)
}
