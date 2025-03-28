use ntex::web;
use std::collections::{HashMap, HashSet};
use std::sync:: Arc;
use serde::{Deserialize, Serialize};
use std:: time::Instant;
use ntex::{fn_service, chain};
use ntex::{channel::oneshot, rt};
use ntex::service::{fn_factory_with_config, fn_shutdown, Service};
use futures::channel::mpsc::{self, UnboundedSender};
use futures::{SinkExt, StreamExt};
use uuid;
use tokio;


#[derive(Debug)]
pub enum ClientMessage {
    Id(String),
    GameState(Room),
    Error(String),
    YourTurn(bool),
}

#[derive(Debug)]
pub enum ServerMessage {
    Connect {
        id: String,
        sender: UnboundedSender<ClientMessage>,
    },
    Disconnect(String),
    JoinGame {
        player_id: String,
        game_id: String,
    },
    MakeMove {
        player_id: String,
        game_id: String,
        position: Vec<usize>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct Room {
    game_id: String,
    players: HashSet<String>,
    board: Vec<Vec<String>>,
    x_turn: bool,
}

#[derive(Deserialize)]
struct WsMessage {
    message_type: String,
    player: Option<String>,
    game_id: Option<String>,
    r#move: Option<Vec<usize>>,
}

pub struct GameServer {
    sessions: HashMap<String, UnboundedSender<ClientMessage>>,
    games: HashMap<String, Room>,
}

impl Default for GameServer {
    fn default() -> Self {
        GameServer {
            sessions: HashMap::new(),
            games: HashMap::new(),
        }
    }
}

impl GameServer {
    fn send_to_player(&self, player_id: &str, msg: ClientMessage) {
        if let Some(addr) = self.sessions.get(player_id) {
            let mut addr = addr.clone();
            rt::spawn(async move {
                let _ = addr.send(msg).await;
            });
        }
    }

    fn notify_game_state(&self, game: &Room) {
        for player_id in &game.players {
            self.send_to_player(
                player_id,
                ClientMessage::GameState(game.clone())
            );
            self.send_to_player(
                player_id,
                ClientMessage::YourTurn(game.x_turn == (player_id == "X"))
            );
        }
    }

    fn handle(&mut self, msg: ServerMessage) {
        match msg {
            ServerMessage::Connect { id, mut sender } => {
                println!("Player {} connected", id);
                self.sessions.insert(id.clone(), sender.clone());
                let _ = sender.send(ClientMessage::Id(id));
            }

            ServerMessage::Disconnect(id) => {
                println!("Player {} disconnected", id);
                self.sessions.remove(&id);
                
                // Remove player from their game
                for game in self.games.values_mut() {
                    game.players.remove(&id);
                }
            }

            ServerMessage::JoinGame { player_id, game_id } => {
                let game = self.games.entry(game_id.clone())
                    .or_insert_with(|| Room {
                        game_id: game_id.clone(),
                        players: HashSet::new(),
                        board: vec![vec!["_".to_string(); 3]; 3],
                        x_turn: true,
                    });

                if game.players.len() < 2 {
                    game.players.insert(player_id);
                    let game_state = game.clone();
                    self.notify_game_state(&game_state);
                } else {
                    self.send_to_player(&player_id, ClientMessage::Error("Game is full".into()));
                }
            }

            ServerMessage::MakeMove { player_id, game_id, position } => {
                if let Some(game) = self.games.get_mut(&game_id) {
                    if !game.players.contains(&player_id) {
                        self.send_to_player(&player_id, ClientMessage::Error("Not in game".into()));
                        return;
                    }

                    let is_x = player_id == "X";
                    if game.x_turn != is_x {
                        self.send_to_player(&player_id, ClientMessage::Error("Not your turn".into()));
                        return;
                    }

                    let [row, col] = position.as_slice() else {
                        self.send_to_player(&player_id, ClientMessage::Error("Invalid move".into()));
                        return;
                    };

                    if game.board[*row][*col] != "_" {
                        self.send_to_player(&player_id, ClientMessage::Error("Invalid move".into()));
                        return;
                    }

                    game.board[*row][*col] = if is_x { "X" } else { "O" }.to_string();
                    game.x_turn = !game.x_turn;
                    let game_state = game.clone();
                    self.notify_game_state(&game_state);
                }
            }
        }
    }
}

pub fn start_game_server() -> UnboundedSender<ServerMessage> {
    let (tx, mut rx) = mpsc::unbounded();

    rt::spawn(async move {
        let mut server = GameServer::default();
        while let Some(msg) = rx.next().await {
            server.handle(msg);
        }
    });

    tx
}

#[derive(Clone)]
struct WsState {
    tx: Arc<tokio::sync::Mutex<UnboundedSender<ServerMessage>>>,
    hb: Instant,
}

async fn ws_service(
    sink: web::ws::WsSink,
    mut server: UnboundedSender<ServerMessage>,
) -> Result<impl Service<web::ws::Frame, Response = Option<web::ws::Message>, Error = std::io::Error>, web::Error> {
    let (client_tx, mut client_rx) = mpsc::unbounded();
    let player_id = uuid::Uuid::new_v4().to_string();

    // Connect player to server
    server.send(ServerMessage::Connect {
        id: player_id.clone(),
        sender: client_tx,
    }).await.unwrap();

    // Spawn task to forward messages from server to websocket
    let sink2 = sink.clone();
    rt::spawn(async move {
        while let Some(msg) = client_rx.next().await {
            match msg {
                ClientMessage::GameState(state) => {
                    let _ = sink2.send(web::ws::Message::Text(
                        serde_json::to_string(&state).unwrap().into()
                    )).await;
                }
                ClientMessage::YourTurn(is_turn) => {
                    let _ = sink2.send(web::ws::Message::Text(
                        format!("{{\"type\":\"turn\",\"is_turn\":{}}}", is_turn).into()
                    )).await;
                }
                _ => {}
            }
        }
    });

    // Handler service for incoming websocket frames
    let (tx, _) = futures::channel::mpsc::unbounded();
    let tx_mutex = Arc::new(tokio::sync::Mutex::new(tx));
    let state = WsState { 
        tx: tx_mutex,
        hb: Instant::now(),
    };
    let state = state.clone();
    let tx = state.tx.clone();
    let hb = Arc::new(tokio::sync::Mutex::new(state.hb));
    let tx_clone = tx.clone();
    let hb_clone = hb.clone();
    let service = fn_service(move |frame| {
        let tx = tx_clone.clone();
        let hb = hb_clone.clone();
        async move {
            match frame {
                web::ws::Frame::Ping(msg) => Ok(Some(web::ws::Message::Pong(msg))),
                web::ws::Frame::Pong(_) => {
                    *hb.lock().await = Instant::now();
                    Ok(None)
                }
                web::ws::Frame::Text(text) => {
                    let text_str = String::from_utf8(Vec::from(text.as_ref())).unwrap();
                    if let Ok(msg) = serde_json::from_str::<WsMessage>(&text_str) {
                        let item = match msg.message_type.as_str() {
                            "join" => {
                                if let (Some(player), Some(game_id)) = (msg.player, msg.game_id) {
                                    tx.lock().await.unbounded_send(ServerMessage::JoinGame {
                                        player_id: player,
                                        game_id,
                                    }).unwrap();
                                    None
                                } else {
                                    None
                                }
                            }
                            "move" => {
                                if let (Some(player), Some(game_id), Some(mv)) = (msg.player, msg.game_id, msg.r#move) {
                                    tx.lock().await.unbounded_send(ServerMessage::MakeMove {
                                        player_id: player,
                                        game_id,
                                        position: mv,
                                    }).unwrap();
                                    None
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        };
                        Ok(item)
                    } else {
                        Ok(None)
                    }
                }
                web::ws::Frame::Close(reason) => Ok(Some(web::ws::Message::Close(reason))),
                _ => Ok(None),
            }
        }
    });

    let (tx, _) = oneshot::channel();
    let on_shutdown = fn_shutdown(move || {
        let _ = tx.send(());
    });

    Ok(chain(service).and_then(on_shutdown))
}


async fn ws_index(
    req: web::HttpRequest,
    server: web::types::State<UnboundedSender<ServerMessage>>,
) -> Result<web::HttpResponse, web::Error> {
    web::ws::start(
        req,
        fn_factory_with_config(move |sink| ws_service(sink, server.get_ref().clone())),
    ).await
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    let server = start_game_server();
    
    web::HttpServer::new(move || {
        web::App::new()
            .state(server.clone())
            .wrap(web::middleware::Logger::default())
            .service(web::resource("/ws/").route(web::get().to(ws_index)))
    })
        .bind(("127.0.0.1", 8080))?
        .run()
        .await
}
