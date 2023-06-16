use std::{
    error::Error,
    sync::{atomic::AtomicBool, Arc},
};

use dotenv::dotenv;
use futures_util::{StreamExt, TryStreamExt};
use kirogpt::{AppData, ChatCompletionRequest, ChatDocument, Message, PromptDocument};
use mongodb::{bson, options::ClientOptions};
use tokio::{main, sync::Mutex};
use twilight_cache_inmemory::{InMemoryCache, ResourceType};
use twilight_gateway::{Event, Intents, Shard, ShardId};
use twilight_http::Client as DiscordClient;
use twilight_model::gateway::payload::incoming::MessageCreate;

#[main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    ctrlc::set_handler(move || {
        println!("Exiting...");
        std::process::exit(0);
    })
    .expect("Error setting Ctrl-C handler");

    dotenv().ok();
    let token = std::env::var("DISCORD_TOKEN")?;

    let mongo_client = get_mongo_client().await?;

    let db = mongo_client.database("kirogpt");

    let prompt_collection = db.collection::<PromptDocument>("prompts");

    let all_prompts = prompt_collection
        .find(None, None)
        .await?
        .try_collect::<Vec<_>>()
        .await?;

    let intents = Intents::GUILD_MESSAGES | Intents::DIRECT_MESSAGES | Intents::MESSAGE_CONTENT;

    let mut shard = Shard::new(ShardId::ONE, token.clone(), intents);

    let http = Arc::new(DiscordClient::new(token));

    let cache = InMemoryCache::builder()
        .resource_types(ResourceType::MESSAGE)
        .build();

    let user_model = http.current_user().await?.model().await?;

    // Get the bot ID
    let bot_id = u64::from(user_model.id);
    let username = user_model.name;

    println!("Got the bot ID. {}", bot_id);
    println!("Hello, {}!", username);

    // New vec u64
    let processing = Arc::new(Mutex::new(Vec::<u64>::new()));

    let app_data = Arc::new(AppData {
        username,
        bot_id,
        all_prompts,
    });

    loop {
        let event = match shard.next_event().await {
            Ok(event) => event,
            Err(error) => {
                tracing::warn!(?error, "error receiving event");

                if error.is_fatal() {
                    eprintln!("error: {}", error);
                    break;
                }

                continue;
            }
        };

        cache.update(&event);

        tokio::spawn(handle_event(
            Arc::clone(&http),
            event,
            Arc::clone(&app_data),
            processing.clone(),
        ));
    }

    println!("done");

    Ok(())
}

async fn handle_event(
    http: Arc<DiscordClient>,
    event: Event,
    app_data: Arc<AppData>,
    processing: Arc<Mutex<Vec<u64>>>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    match event {
        Event::Ready(ready) => {
            println!("{} is ready", ready.user.name);
        }

        Event::MessageCreate(message) => {
            // Check if the message is a reply to a message sent by the bot

            let message = Arc::new(message);

            let reply = message.referenced_message.as_ref().and_then(|msg| {
                if msg.author.id == app_data.bot_id {
                    Some(msg)
                } else {
                    None
                }
            });

            if reply.is_some() {
                // Get message history

                println!("Have a reply: {:?}", reply);

                let reply = reply.unwrap();

                if processing.lock().await.contains(&u64::from(reply.id)) {
                    println!("Already processing message");
                    return Ok(());
                }

                let mongo_client = get_mongo_client().await?;

                println!("Got mongo client");

                let db = mongo_client.database("kirogpt");

                println!("Got database");

                let collection = db.collection::<ChatDocument>("messages");

                let reply_id = reply.id.to_string();

                println!("Getting history...");

                let history = collection
                    .find(
                        Some(bson::doc! {
                            "id": reply_id.clone()
                        }),
                        None,
                    )
                    .await?;

                // Check if the history is empty

                let history = history.try_collect::<Vec<_>>().await?;

                if history.is_empty() {
                    println!("History is empty");
                    return Ok(());
                }

                println!("Got history: {:?}", history);

                handle_message(
                    Arc::clone(&http),
                    Arc::clone(&message),
                    Arc::clone(&app_data),
                    Arc::clone(&processing),
                    Some(history),
                    Some(reply_id),
                )
                .await?;

                println!("Handled message");

                return Ok(());
            }

            // Check if the message has a mention to the bot
            let mention = message
                .mentions
                .iter()
                .find(|mention| mention.id == app_data.bot_id);

            if mention.is_some() {
                handle_message(
                    Arc::clone(&http),
                    Arc::clone(&message),
                    Arc::clone(&app_data),
                    Arc::clone(&processing),
                    None,
                    None,
                )
                .await?;

                // let bot_response = http
                //     .create_message(message.channel_id)
                //     .content("You mentioned me!")?
                //     .await?;

                // let id = bot_response.model().await?.id;

                return Ok(());
            }

            if message.content == "!ping" {
                http.create_message(message.channel_id)
                    .content("Pong!")?
                    .await?;
            }
        }
        _ => {}
    }

    Ok(())
}

async fn get_mongo_client() -> Result<mongodb::Client, Box<dyn Error + Send + Sync>> {
    let mongo_url = std::env::var("MONGO_URL")?;

    let mut client_options = ClientOptions::parse(&mongo_url).await?;

    client_options.app_name = Some("kirogpt".to_string());

    Ok::<mongodb::Client, Box<dyn Error + Send + Sync>>(mongodb::Client::with_options(
        client_options,
    )?)
}

async fn handle_message(
    http: Arc<DiscordClient>,
    message: Arc<Box<MessageCreate>>,
    app_data: Arc<AppData>,
    processing: Arc<Mutex<Vec<u64>>>,
    history: Option<Vec<ChatDocument>>,
    reply_id: Option<String>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let client = reqwest::Client::new();

    let mut headers = reqwest::header::HeaderMap::new();

    // Authorization header

    let gpt_token = std::env::var("GPT_TOKEN")?;

    headers.insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_str(&format!("Bearer {}", gpt_token))?,
    );

    // Content-Type header

    headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );

    let mut messages = vec![];

    if history.is_some() {
        let history = history.clone().unwrap();

        messages.extend(history.iter().map(|doc| doc.messages.clone()).flatten());
    }

    // Manipulating string inline

    let mut user_message = message.content.clone();

    user_message = user_message.replace(&format!("<@{}>", app_data.bot_id), "");

    user_message = user_message.replace(", ", "");

    let expert_prompt = app_data
        .all_prompts
        .iter()
        .find(|prompt| prompt.prompt_id == "expert")
        .ok_or("No expert prompt")?;

    let jb_prompt = app_data
        .all_prompts
        .iter()
        .find(|prompt| prompt.prompt_id == "jb")
        .ok_or("No jb prompt")?;

    let uwu_prompt = app_data
        .all_prompts
        .iter()
        .find(|prompt| prompt.prompt_id == "uwu")
        .ok_or("No uwu prompt")?;

    user_message = user_message.replace("!expert", expert_prompt.prompt.as_str());

    user_message = user_message.replace("!jb", jb_prompt.prompt.as_str());

    if user_message.contains("!uwu") {
        let arguments = user_message.split_whitespace().collect::<Vec<_>>();

        let last_argument = arguments.last().ok_or("No last argument")?;

        if last_argument.contains("\"") {
            // if last_argument.split_whitespace().count() <= 1 {
            //     http.create_message(message.channel_id)
            //         .content("You need to provide a name.")?
            //         .reply(message.id)
            //         .await?;

            //     return Ok(());
            // }

            // Trim the \" if it exists
            let last_argument = last_argument.replace("\"", "");

            let name = last_argument
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");

            // Replace {FIRST_NAME} with the first name, and {LAST_NAME} with the last name

            let uwu_prompt = uwu_prompt.prompt.replace("{FIRST_NAME}", "{NAME}");

            let uwu_prompt = uwu_prompt.replace("{FULL_NAME}", "{NAME}");

            let uwu_prompt = uwu_prompt.replace("{LAST_NAME}", "{NAME}");

            let uwu_prompt = uwu_prompt.replace("{NAME}", &name);

            user_message = user_message.replace("!uwu", &uwu_prompt);
        } else {
            let bot_resp = http
                .create_message(message.channel_id)
                .content("You need to provide a name.")?
                .reply(message.id)
                .await?;

            tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

                http.delete_message(message.channel_id, bot_resp.model().await.unwrap().id)
                    .await
                    .unwrap();
            });

            return Ok(());
        }
    }

    messages.push(kirogpt::Message {
        role: "user".to_string(),
        content: user_message,
    });

    // Send typing indicator

    let finished = Arc::new(AtomicBool::new(false));

    processing.lock().await.push(u64::from(message.id));

    tokio::spawn({
        let finished = Arc::clone(&finished);
        let http = Arc::clone(&http);
        let message = Arc::clone(&message);
        async move {
            while !finished.load(std::sync::atomic::Ordering::SeqCst) {
                http.create_typing_trigger(message.channel_id)
                    .await
                    .unwrap();

                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
        }
    });

    let res = client
        .post("https://api.openai.com/v1/chat/completions")
        .headers(headers)
        .json(&kirogpt::ChatCompletionRequest {
            model: "gpt-3.5-turbo".to_string(),
            messages: messages.clone(),
        })
        .send()
        .await?;

    finished.store(true, std::sync::atomic::Ordering::SeqCst);

    processing
        .lock()
        .await
        .retain(|id| *id != u64::from(message.id));

    println!("Response: {:?}", res);

    let response: kirogpt::ChatCompletionResponse = res.json().await?;

    let mut response = response.choices;

    let response = response.pop().ok_or("No response")?;

    let response = response.message.content;

    println!("Response: {}", response);

    let bot_msg = http
        .create_message(message.channel_id)
        .content(&response)?
        .reply(message.id)
        .await?;

    let bot_msg_id = u64::from(bot_msg.model().await?.id);

    messages.push(kirogpt::Message {
        role: "assistant".to_string(),
        content: response,
    });

    let mongo_client = get_mongo_client().await?;

    let db = mongo_client.database("kirogpt");

    let collection = db.collection("messages");

    let doc = bson::to_document(&kirogpt::ChatDocument {
        id: bot_msg_id.to_string(),
        messages: messages.clone(),
    })?;

    if history.is_none() {
        let res = collection.insert_one(doc.clone(), None).await?;

        println!("Inserted document with id {:?}", res.inserted_id);
    }

    if reply_id.is_some() {
        println!("hi");

        println!("messages: {:?}", messages);

        let id = reply_id.unwrap();

        println!("{}", id);

        let new_doc = bson::to_document(&kirogpt::ChatDocument {
            id: bot_msg_id.to_string(),
            messages: messages.clone(),
        })?;

        collection
            .find_one_and_update(
                bson::doc! {
                    "id": id
                },
                bson::doc! {
                    "$set": new_doc
                },
                None,
            )
            .await?;
    }

    Ok(())
}
