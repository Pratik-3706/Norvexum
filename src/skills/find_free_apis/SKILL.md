---
name: find_free_apis
description: How to search for, evaluate, and integrate free APIs without hitting paywalls.
---

# Finding and Using Free APIs

When a task requires fetching external data (like weather, crypto prices, jokes, or images) and you need to find a free API, follow these guidelines:

## 1. Search Strategies
- **Queries to use:** Search for "public APIs", "free APIs without auth", or specifically "free [category] API github".
- **Awesome Lists:** One of the best resources is the `public-apis` GitHub repository. Search for `github public-apis` to find a curated list of free APIs.
- **Avoid:** Avoid searching just for "[category] API" as the top results are almost always paid services with aggressive SEO (like RapidAPI, which requires credit cards).

## 2. Red Flags (What to Avoid)
- **RapidAPI / API-Ninja:** Avoid these wrapper platforms. They require the user to sign up, input a credit card, and get an API key. 
- **Freemium Limits:** If an API says "Free tier: 100 requests/month" but requires a credit card to unlock, DO NOT USE IT.
- **Complex Auth:** Prefer APIs that either require no authentication at all, or just a simple free API key via email (no credit card).

## 3. Reading the Docs
- Once you find a potential API, read the documentation carefully.
- Look for the exact base URL and endpoint paths.
- Check if they require specific headers (e.g., `User-Agent`, `Accept: application/json`).
- If an API key is required, STOP and ask the user to procure one and add it to their environment variables before you attempt to write code.

## 4. Integration
- Always test the API using your `web_fetch` or `run_command` (with `curl`) tool before writing the integration code.
- Ensure your code handles HTTP errors gracefully, as free APIs often have rate limits or go down unexpectedly.
