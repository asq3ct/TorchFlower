//! Chat-controlled miner.
//!
//! ```text
//! cargo run -p torchflower-bot --example miner -- 127.0.0.1 19132 Miner [canonical_block_states.nbt]
//! ```
//!
//! The block palette for the server's protocol is embedded, so the last
//! argument is only needed for servers with a non-vanilla palette.
//!
//! Only connect to servers you own or have permission to test on. Commands in
//! chat: `!mine <block>`, `!come`, `!follow`, `!stop`, `!pos`, `!eat`,
//! `!collect`, `!menu <button>`.

use std::sync::Arc;

use torchflower_bot::{
    BlockPos, Bot, BotConfig, BotEvent, BotResult, DigOptions, GoalGetToBlock, GoalNear,
};

#[tokio::main]
async fn main() -> BotResult<()> {
    let args: Vec<String> = std::env::args().collect();
    let host = args.get(1).cloned().unwrap_or_else(|| "127.0.0.1".into());
    let port = args.get(2).and_then(|p| p.parse().ok()).unwrap_or(19132);
    let name = args.get(3).cloned().unwrap_or_else(|| "Miner".into());
    let mut config = BotConfig::offline(host, port, name);
    if let Some(path) = args.get(4) {
        let bytes =
            std::fs::read(path).map_err(|e| torchflower_bot::BotError::Other(e.to_string()))?;
        config.canonical_block_states = Some(Arc::from(bytes.into_boxed_slice()));
    }

    let bot = Bot::connect(config).await?;
    println!("spawned at {:?}", bot.position());

    bot.on_chat(|bot, sender, message| async move {
        let mut words = message.split_whitespace();
        match words.next() {
            Some("!mine") => {
                let name = words.next().unwrap_or("iron_ore");
                let Some(target) = bot.find_block(name, 32) else {
                    return bot.chat(format!("no {name} nearby")).await;
                };
                bot.navigate_to(GoalGetToBlock(target.pos)).await?;
                bot.dig_with(
                    target.pos,
                    DigOptions {
                        collect_drops: true,
                    },
                )
                .await?;
                bot.chat(format!("{name} mined!")).await?;
            }
            Some("!come") => {
                if let Some(p) = bot.player(&sender) {
                    let pos = BlockPos::from_f64(p.position.x, p.position.y, p.position.z);
                    bot.navigate_to(GoalNear::new(pos, 2)).await?;
                }
            }
            Some("!follow") => {
                if let Some(p) = bot.player(&sender) {
                    let bot = bot.clone();
                    tokio::spawn(async move { bot.follow(p.runtime_id, 2.0).await });
                }
            }
            Some("!stop") => bot.stop()?,
            Some("!eat") => {
                let id = bot.eat().await?;
                let food = bot.item_name(id).unwrap_or_else(|| id.to_string());
                bot.chat(format!("ate {food}")).await?;
            }
            Some("!collect") => {
                let n = bot.collect_drops(8.0).await?;
                bot.chat(format!("picked up {n} stacks")).await?;
            }
            Some("!menu") => {
                // Clicks a button of the most recent server form.
                let button = words.next().and_then(|b| b.parse().ok()).unwrap_or(0);
                if let Some((form_id, _)) = bot.open_forms().pop() {
                    bot.click_form_button(form_id, button).await?;
                }
            }
            Some("!pos") => {
                let p = bot.position();
                bot.chat(format!(
                    "{:.1} {:.1} {:.1} (state {} KiB, {} corrections)",
                    p.x,
                    p.y,
                    p.z,
                    bot.heap_bytes() / 1024,
                    bot.with_state(|s| s.corrections)
                ))
                .await?;
            }
            _ => {}
        }
        Ok(())
    });

    let mut events = bot.events();
    while let Ok(ev) = events.recv().await {
        match ev {
            BotEvent::Disconnected(r) => {
                println!("disconnected: {r}");
                break;
            }
            BotEvent::FormRequest { form_id, data } => {
                println!(
                    "form {form_id}: {}",
                    data.chars().take(120).collect::<String>()
                );
            }
            BotEvent::HandlerError(e) => eprintln!("handler error: {e}"),
            _ => {}
        }
    }
    Ok(())
}
