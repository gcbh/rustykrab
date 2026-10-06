//! Operator snapshots and health checks; a thin client of the monitor API.
use rustykrab_gateway::monitor_routes::MonitorReply;
use std::{path::Path, time::Duration};

const USAGE: &str = "usage: rustykrab monitor [--json] [--check]\n\
  --json   machine-readable snapshot\n\
  --check  exit 1 for degraded or critical health; exit 0 only for healthy\n\
Open /monitor.html on the daemon for the live dashboard.";

pub async fn run(data_dir: &Path, args: &[String]) -> anyhow::Result<()> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "help"))
    {
        println!("{USAGE}");
        return Ok(());
    }
    if args
        .iter()
        .any(|arg| !matches!(arg.as_str(), "--json" | "--check"))
    {
        anyhow::bail!("{USAGE}");
    }
    let (base, http) = crate::daemon_client::connect(data_dir, Duration::from_secs(15)).await?;
    let response = http.get(base.join("/api/monitor")?).send().await?;
    if !response.status().is_success() {
        anyhow::bail!("monitor request failed ({})", response.status());
    }
    let reply: MonitorReply = response.json().await?;
    if args.iter().any(|a| a == "--json") {
        println!("{}", serde_json::to_string_pretty(&reply)?);
    } else {
        print!("{}", render(&reply));
    }
    if args.iter().any(|a| a == "--check") && reply.health != "healthy" {
        std::process::exit(1);
    }
    Ok(())
}

fn render(reply: &MonitorReply) -> String {
    let work = &reply.work;
    let mut out = format!(
        "Agents: {} | {} | observed {}\n",
        reply.health,
        reply.version,
        work.captured_at.to_rfc3339()
    );
    out.push_str(&format!(
        "Work: {}; archive: {}; waiting questions: {}; undelivered notices: {}\n",
        work.counts
            .iter()
            .map(|(s, n)| format!("{n} {s}"))
            .collect::<Vec<_>>()
            .join(", "),
        work.total_archived,
        work.pending_questions,
        work.pending_notices
    ));
    for worker in &reply.workers {
        let runs: Vec<_> = work
            .items
            .iter()
            .filter(|row| row.lease.as_ref().is_some_and(|l| l.worker == worker.name))
            .collect();
        out.push_str(&format!(
            "\n{} [{}] {} | {} active\n",
            worker.name,
            worker.kind,
            worker.health,
            work.active_by_worker
                .get(&worker.name)
                .copied()
                .unwrap_or(0)
        ));
        for row in runs {
            out.push_str(&format!(
                "  {} {} ({})\n",
                row.item.id, row.item.title, row.item.status
            ));
        }
    }
    for alert in &reply.alerts {
        out.push_str(&format!(
            "\n{} {}: {}\n",
            alert.severity, alert.code, alert.message
        ));
    }
    if work.items_truncated {
        out.push_str("\nDisplayed work is limited; aggregate counts cover all durable rows.\n");
    }
    out
}
