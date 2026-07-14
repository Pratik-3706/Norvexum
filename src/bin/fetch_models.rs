
use reqwest;
use serde_json::Value;
use keyring::Entry;

fn get_secure_key(key: &str) -> Option<String> {
    let entry = Entry::new("norvexum", key).ok()?;
    entry.get_password().ok()
}

#[tokio::main]
async fn main() {
    let client = reqwest::Client::new();
    let res = client.get("https://api.aicredits.in/v1/models")
        .send()
        .await
        .unwrap();
        
    let text = res.text().await.unwrap();
    if let Ok(json) = serde_json::from_str::<Value>(&text) {
        if let Some(models) = json.get("data").and_then(|d| d.as_array()) {
            if let Some(first) = models.first() {
                println!("{}", serde_json::to_string_pretty(first).unwrap());
            } else {
                println!("No models found in data array.");
            }
        } else {
            println!("Response did not have 'data' array. {}", text);
        }
    } else {
        println!("Invalid JSON: {}", text);
    }
}

