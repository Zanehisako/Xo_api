use ntex:: web;
use std::collections::HashSet;
use std::sync::{Mutex, Arc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
struct MoveRequest {
    player: String,
    r#move: Vec<usize>,  // Using r#move since 'move' is a keyword
}

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

#[web::get("/")]
async fn hello() -> impl web::Responder {
    web::HttpResponse::Ok().body("Hello there!")
}

#[web::post("/join")]
async fn join_game(
    data: web::types::State<Arc<Mutex<Option<Room>>>>,
    player_type: String,
) -> impl web::Responder {
    let mut room = data.lock().unwrap();
    
    if let Some(r) = room.as_mut() {
        if r.players.len() < 2 {
            r.players.insert(player_type.clone());
            web::HttpResponse::Ok().json(&JoinResponse {
                player_type,
                game_ready: r.players.len() == 2,
            })
        } else {
            web::HttpResponse::BadRequest().body("Room is full")
        }
    } else {
        let mut new_room = Room::new();
        new_room.players.insert(player_type.clone());
        *room = Some(new_room);
        web::HttpResponse::Ok().json(&JoinResponse {
            player_type,
            game_ready: false,
        })
    }
}

#[web::post("/move")]
async fn make_move(
    data: web::types::State<Arc<Mutex<Option<Room>>>>,
    move_req: web::types::Json<MoveRequest>,
) -> impl web::Responder {
    let mut room = data.lock().unwrap();
    
    if let Some(r) = room.as_mut() {
        if !r.players.contains(&move_req.player) {
            return web::HttpResponse::BadRequest().body("Player not in game");
        }
        
        let row = move_req.r#move[0];
        let col = move_req.r#move[1];
        
        if r.board[row][col] == "X" || r.board[row][col] == "O" {
            return web::HttpResponse::BadRequest().body("Invalid move");
        }
        
        r.board[row][col] = if r.x_turn { "X" } else { "O" }.to_string();
        r.x_turn = !r.x_turn;
        
        web::HttpResponse::Ok().json(r)
    } else {
        web::HttpResponse::BadRequest().body("No active game")
    }
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    let game_state = Arc::new(Mutex::new(None::<Room>));
    
    web::HttpServer::new(move || {
        web::App::new()
            .state(game_state.clone())
            .service(join_game)
            .service(make_move)
    })
    .bind(("127.0.0.1", 8080))?
    .run()
    .await
}
