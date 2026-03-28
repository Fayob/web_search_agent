use web_search_agent::{agent::SearchAgent, config::Config};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("research_agent=info".parse()?)
        )
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .init();

    let config = Config::from_env()?.into_arc();
    let agent = SearchAgent::new(config);

    // let question = std::env::args().nth(1)
    //     .unwrap_or_else(|| {
    //         "What are the latest zero-knowledge proof implementations \
    //          in Ethereum scaling solutions?".to_string()
    //     });

    let question = "Compare London and Ontario's weather, 
            and also give me the current btc price in yen";

    tracing::info!(question = %question, "starting search agent");

    let result = agent.run(&question).await?;

    match result.report {
        Some(report) => {
            println!("\n{}", "=".repeat(60));
            println!("RESEARCH REPORT");
            println!("{}", "=".repeat(60));
            println!("Question:   {}", report.question);
            println!("Confidence: {}", report.confidence);
            println!("\nAnswer:\n{}", report.answer);

            println!("\nKey Findings:");
            for f in &report.key_findings {
                println!("  • {}", f);
            }

            println!("\nSources:");
            for s in &report.sources {
                println!("  [{}]\n  {}\n  {}", s.title, s.url, s.snippet);
            }

            println!("\nSearched for: {}", report.search_queries.join(", "));
            println!("Limitations: {}", report.limitations);
        }
        None => {
            println!("\n[!] Agent completed without a parseable report.");
            println!("    Termination reason: {:?}", result.termination_reason);
            println!("    Iterations: {}", result.iterations);
            println!("    Tool calls: {}", result.tool_calls.len());
        }
    }

   Ok(())
}
