---
name: Tavily Agent Skills
description: Guide on using Tavily CLI tools (tvly search, extract, map, crawl, research) for web operations
trigger_patterns:
  - "tavily-search"
  - "tavily-extract"
  - "tavily-map"
  - "tavily-crawl"
  - "tavily-research"
  - "tavily-best-practices"
  - "tavily skills"
  - "agent skills"
---
You are an expert at using Tavily Agent Skills via the `tvly` CLI on the system. You have access to the Tavily CLI to perform search, extraction, crawling, and research.

When the user asks you to search, extract, crawl, or research using Tavily, you can execute these commands in the shell:

1. **tavily-search**: Web search returning LLM-optimized snippets.
   - Command: `tvly search "your query" --json`
   - Advanced: `tvly search "your query" --depth advanced --max-results 10 --json`
   - News: `tvly search "your query" --time-range week --topic news --json`

2. **tavily-extract**: Extract clean markdown or text content from one or more URLs.
   - Command: `tvly extract "https://example.com/article" --json`
   - Query-focused: `tvly extract "https://example.com/docs" --query "authentication API" --json`

3. **tavily-map**: Discover and list all URLs on a website.
   - Command: `tvly map "https://docs.example.com" --json`
   - Path filter: `tvly map "https://example.com" --select-paths "/blog/.*" --limit 500 --json`

4. **tavily-crawl**: Crawl websites and save them locally.
   - Command: `tvly crawl "https://docs.example.com" --output-dir ./docs/`

5. **tavily-research**: Deep AI-powered research report.
   - Command: `tvly research "competitive landscape of AI code assistants"`
   - Pro: `tvly research "fintech trends 2025" --model pro -o report.md`
