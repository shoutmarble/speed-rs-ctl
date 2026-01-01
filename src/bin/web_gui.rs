use iced::widget::{button, column, text};
use iced::{Element, Subscription, Theme};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
enum Message {
    ToggleServer,
    Tick,
}

struct WebGui {
    counter: Arc<Mutex<i32>>,
    server_running: bool,
    stop_tx: Option<mpsc::UnboundedSender<()>>,
}

fn new() -> WebGui {
    let mut state = WebGui {
        counter: Arc::new(Mutex::new(0)),
        server_running: false,
        stop_tx: None,
    };

    // Start the server automatically on launch.
    start_server(&mut state);

    state
}

fn start_server(state: &mut WebGui) {
    if state.server_running {
        return;
    }

    let counter = Arc::clone(&state.counter);
    let (tx, mut rx) = mpsc::unbounded_channel();
    state.stop_tx = Some(tx);

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use warp::Filter;

                        println!("web_gui: listening on http://0.0.0.0:8080");

            let counter_get = Arc::clone(&counter);
            let get_counter = warp::path("counter")
                .and(warp::get())
                .map(move || {
                    let count = *counter_get.lock().unwrap();
                    warp::reply::json(&serde_json::json!({ "counter": count }))
                });

            let counter_post = Arc::clone(&counter);
            let post_counter = warp::path("counter")
                .and(warp::post())
                .and(warp::body::json::<serde_json::Value>())
                .map(move |body: serde_json::Value| {
                                if let Some(count) = body
                                    .get("count")
                                    .or_else(|| body.get("counter"))
                                    .and_then(|v| v.as_i64())
                                {
                        *counter_post.lock().unwrap() = count as i32;
                                    println!("web_gui: POST /counter = {}", count);
                        warp::reply::json(&serde_json::json!({ "status": "ok" }))
                    } else {
                        warp::reply::json(&serde_json::json!({ "error": "invalid count" }))
                    }
                });

            let routes = get_counter.or(post_counter);

            let addr = std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, 8080));
            let listener = match tokio::net::TcpListener::bind(addr).await {
                Ok(listener) => listener,
                Err(e) => {
                    eprintln!("web_gui: failed to bind {:?}: {}", addr, e);
                    return;
                }
            };

            println!("web_gui: bound to http://0.0.0.0:8080");
            warp::serve(routes)
                .incoming(listener)
                .graceful(async move {
                    let _ = rx.recv().await;
                })
                .run()
                .await;
        });
    });

    state.server_running = true;
}

fn update(state: &mut WebGui, message: Message) {
    match message {
        Message::ToggleServer => {
            if state.server_running {
                if let Some(tx) = state.stop_tx.take() {
                    let _ = tx.send(());
                }
                state.server_running = false;
            } else {
                start_server(state);
            }
        }
        Message::Tick => {}
    }
}

fn view(state: &WebGui) -> Element<'_, Message> {
    let counter_text = text(format!("Counter: {}", *state.counter.lock().unwrap()));
    let button_text = if state.server_running {
        "Stop Server"
    } else {
        "Start Server"
    };

    column![
        counter_text,
        button(button_text).on_press(Message::ToggleServer)
    ]
    .into()
}

fn subscription(_state: &WebGui) -> Subscription<Message> {
    iced::time::every(std::time::Duration::from_secs(1)).map(|_| Message::Tick)
}

fn main() -> iced::Result {
    iced::application(new, update, view)
        .title("Web GUI")
        .theme(Theme::Dark)
        .window(iced::window::Settings {
            size: iced::Size::new(1024.0 / 5.0, 768.0 / 5.0),
            ..Default::default()
        })
        .subscription(subscription)
        .run()
}