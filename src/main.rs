use ntex::{web, util::Bytes};
use std::collections::HashSet;
use std::sync::{Mutex, Arc};
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, rc::Rc, time::Duration, time::Instant};
use ntex::{fn_service, chain};
use ntex::{channel::oneshot, rt, time};
use futures::future::{ready, select, Either};
use ntex::service::{fn_factory_with_config, fn_shutdown, Service};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);


#[derive(Debug, Clone, Serialize)]
struct Room {
    players: HashSet<String>,
    board: Vec<Vec<String>>,  // Changed from Option<bool> to String
    x_turn: bool,
}

impl Room {
    fn new() -> Self {
        // Initialize board with numbers 1-9
        let board = vec![
            vec!["1".to_string(), "2".to_string(), "3".to_string()],
            vec!["4".to_string(), "5".to_string(), "6".to_string()],
            vec!["7".to_string(), "8".to_string(), "9".to_string()],
        ];
        
        Room {
            players: HashSet::new(),
            board,
            x_turn: true,
        }
    }
}

#[derive(Serialize)]
struct JoinResponse {
    player_type: String,
    game_ready: bool,
}

#[derive(Deserialize)]
struct WsMessage {
    message_type: String,  // "join" or "move"
    player: Option<String>,
    r#move: Option<Vec<usize>>,
}

struct WsState {
    hb: Instant,
    room: web::types::State<Arc<Mutex<Option<Room>>>>,
}

async fn ws_service(
    sink: web::ws::WsSink,
    room: web::types::State<Arc<Mutex<Option<Room>>>>,
) -> Result<impl Service<web::ws::Frame, Response = Option<web::ws::Message>, Error = std::io::Error>, web::Error> {
    let state = Rc::new(RefCell::new(WsState { 
        hb: Instant::now(),
        room,
    }));

    let (tx, rx) = oneshot::channel();

    // Start heartbeat task
    rt::spawn(heartbeat(state.clone(), sink, rx));

    // Handler service for incoming websocket frames
    let service = fn_service(move |frame| {
        let state = state.clone();
        let item = match frame {
            web::ws::Frame::Ping(msg) => {
                state.borrow_mut().hb = Instant::now();
                Some(web::ws::Message::Pong(msg))
            }
            web::ws::Frame::Pong(_) => {
                state.borrow_mut().hb = Instant::now();
                None
            }
            web::ws::Frame::Text(text) => {
                let text_str = String::from_utf8(Vec::from(text.as_ref())).unwrap();
                if let Ok(msg) = serde_json::from_str::<WsMessage>(&text_str) {
                    let state_ref = state.borrow();
                    let mut room = state_ref.room.lock().unwrap();
                    
                    match msg.message_type.as_str() {
                        "join" => {
                            if let Some(r) = room.as_mut() {
                                if r.players.len() < 2 {
                                    if let Some(player) = msg.player {
                                        r.players.insert(player.clone());
                                        return ready(Ok(Some(web::ws::Message::Text(
                                            serde_json::to_string(&JoinResponse {
                                                player_type: player,
                                                game_ready: r.players.len() == 2,
                                            }).unwrap().into(),
                                        ))));
                                    }
                                }
                            } else {
                                let mut new_room = Room::new();
                                if let Some(player) = msg.player {
                                    new_room.players.insert(player.clone());
                                    *room = Some(new_room);
                                    return ready(Ok(Some(web::ws::Message::Text(
                                        serde_json::to_string(&JoinResponse {
                                            player_type: player,
                                            game_ready: false,
                                        }).unwrap().into(),
                                    ))));
                                }
                            }
                            None
                        }
                        "move" => {
                            if let Some(r) = room.as_mut() {
                                if let (Some(player), Some(mv)) = (msg.player, msg.r#move) {
                                    if !r.players.contains(&player) {
                                        return ready(Ok(Some(web::ws::Message::Text(
                                            "Player not in game".into()
                                        ))));
                                    }
                                    
                                    let row = mv[0];
                                    let col = mv[1];
                                    
                                    if r.board[row][col] == "X" || r.board[row][col] == "O" {
                                        return ready(Ok(Some(web::ws::Message::Text(
                                            "Invalid move".into()
                                        ))));
                                    }
                                    
                                    r.board[row][col] = if r.x_turn { "X" } else { "O" }.to_string();
                                    r.x_turn = !r.x_turn;
                                    
                                    return ready(Ok(Some(web::ws::Message::Text(
                                        serde_json::to_string(&r).unwrap().into()
                                    ))));
                                }
                            }
                            return ready(Ok(Some(web::ws::Message::Text("No active game".into()))));
                        }
                        _ => None,
                    }
                } else {
                    None
                }
            }
            web::ws::Frame::Close(reason) => Some(web::ws::Message::Close(reason)),
            _ => None,
        };
        ready(Ok(item))
    });

    let on_shutdown = fn_shutdown(move || {
        let _ = tx.send(());
    });

    Ok(chain(service).and_then(on_shutdown))
}

async fn heartbeat(
    state: Rc<RefCell<WsState>>,
    sink: web::ws::WsSink,
    mut rx: oneshot::Receiver<()>,
) {
    loop {
        match select(Box::pin(time::sleep(HEARTBEAT_INTERVAL)), &mut rx).await {
            Either::Left(_) => {
                if Instant::now().duration_since(state.borrow().hb) > CLIENT_TIMEOUT {
                    println!("Websocket Client heartbeat failed, disconnecting!");
                    return;
                }
                if sink.send(web::ws::Message::Ping(Bytes::default())).await.is_err() {
                    return;
                }
            }
            Either::Right(_) => {
                println!("Connection is dropped, stop heartbeat task");
                return;
            }
        }
    }
}

async fn ws_index(
    req: web::HttpRequest,
    room: web::types::State<Arc<Mutex<Option<Room>>>>,
) -> Result<web::HttpResponse, web::Error> {
    web::ws::start(
        req,
        fn_factory_with_config(move |sink| ws_service(sink, room.clone())),
    ).await
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    let game_state = Arc::new(Mutex::new(None::<Room>));
    
    web::HttpServer::new(move || {
        web::App::new()
            .state(game_state.clone())
            .wrap(web::middleware::Logger::default())
            .service(web::resource("/ws/").route(web::get().to(ws_index)))
    })
        .bind(("127.0.0.1", 8080))?
        .run()
        .await
}
