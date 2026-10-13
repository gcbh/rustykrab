//! Native login and usage observation only: no inference or credential reads.
//! cargo run -p rustykrab-providers --example subscription_status -- COMMAND PROFILE MODEL
use rustykrab_providers::ClaudeMaxRuntime;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 3 {
        eprintln!(
            "usage: subscription_status COMMAND PROFILE MODEL (PROFILE=- selects default login)"
        );
        std::process::exit(2);
    }
    let profile = (args[1] != "-").then(|| args[1].clone().into());
    let runtime = ClaudeMaxRuntime::new(args[0].clone().into(), profile).with_model(Some(&args[2]));
    let result = runtime.verify_login(true).await;
    println!(
        "{}",
        serde_json::to_string_pretty(&runtime.status()).unwrap()
    );
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
