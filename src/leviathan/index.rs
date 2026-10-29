//! The index briefing and per-message recall, as the model sees them.
//!
//! - **Briefing.** A system-prompt section summarising what is indexed
//!   (record and group counts, largest groups, filter fields, date range)
//!   and how to search it, from `describe`. When nothing is indexed yet and
//!   the agent can index, it says how.
//! - **Recall.** Before the first model call of a turn, the user's message is
//!   searched and the top cards are attached to that message as a
//!   `<workspace_recall>` block. The block is persisted with the turn, so a
//!   resumed session replays exactly what the model saw.

use crate::json::Json;
use crate::util;

#[derive(Clone)]
pub struct Briefing {
    pub text: String,
    pub record_count: i64,
}

/// What the model calls each tool, where it has it.
pub struct ToolNames {
    pub search: Option<String>,
    pub get: Option<String>,
    pub resolve: Option<String>,
    pub aggregate: Option<String>,
    pub index: Option<String>,
    /// The model's names differ from Leviathan's own (`search`), which
    /// Leviathan's results refer to.
    pub renamed: bool,
}

fn plural(noun: &str, n: i64) -> String {
    if n == 1 { noun.to_string() } else if noun.ends_with('s') { format!("{noun}es") } else { format!("{noun}s") }
}

/// The search query for a message: a leading bracketed context line (a host
/// may prepend one, such as `[Today is ...]`) is dropped and whitespace is
/// collapsed.
pub fn recall_query(message: &str) -> String {
    let mut text = message.trim();
    if text.starts_with('[')
        && let Some(end) = text.find("]\n")
    {
        text = text[end + 1..].trim();
    }
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    util::truncate(&collapsed, 400, "").to_string()
}

const LARGE_DATA: &str = "Treat the index as the way into the data: it answers in a few hundred tokens whatever its \
size, so search it before reading files, and never read or grep a large dataset whole.";

/// The briefing when no data index exists yet.
pub fn render_empty(names: &ToolNames) -> String {
    let mut out = String::from("# Data (Leviathan)\nNo dataset is indexed yet.");
    if let Some(index) = &names.index {
        out.push_str(&format!(
            " When the user gives you data (files, exports, database dumps, logs), index it with `{index}` first and \
             then work from search results. {LARGE_DATA}"
        ));
    }
    out.push('\n');
    out
}

pub fn render_briefing(describe: &Json, names: &ToolNames) -> String {
    let about = describe.get("about");
    let record = about.get("record").str_or("record");
    let group = about.get("group").str_or("group");
    let records = describe.get("record_count").as_i64().unwrap_or(0);
    let groups = describe.get("group_count").as_i64().unwrap_or(0);
    let mut out = String::from("# Workspace index (Leviathan)\n");
    if records == 0 {
        if names.index.is_some() {
            return render_empty(names);
        }
        out.push_str("The workspace index is empty: nothing has been uploaded or connected yet, so search will find nothing. Say so if the user asks about their data.\n");
        return out;
    }
    if let Some(name) = about.get("name").as_str() {
        out.push_str(&format!("Index: {name}."));
        if let Some(description) = about.get("description").as_str() {
            out.push_str(&format!(" {description}"));
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "It holds {records} {} across {groups} {}",
        plural(record, records),
        plural(group, groups)
    ));
    // Dates the index could not parse are stored raw: cut at characters.
    let day = |d: &str| d.chars().take(10).collect::<String>();
    match (describe.get("date_min").as_str(), describe.get("date_max").as_str()) {
        (Some(min), Some(max)) => out.push_str(&format!(", dated {} to {}.\n", day(min), day(max))),
        _ => out.push_str(".\n"),
    }
    let kinds: Vec<String> = describe
        .get("kinds")
        .items()
        .iter()
        .map(|k| {
            let records = k.get("records").as_i64().unwrap_or(0);
            let fields = k.get("fields");
            let mut parts = vec![format!("{records} {}", plural(k.get("record").str_or("record"), records))];
            if let Some(g) = fields.get("group").as_str() {
                parts.push(format!("grouped by {g}"));
            }
            if let Some(d) = fields.get("date").at(0).as_str() {
                parts.push(format!("dated by {d}"));
            }
            format!("{} ({})", k.get("name").str_or("?"), parts.join(", "))
        })
        .collect();
    if !kinds.is_empty() {
        out.push_str(&format!(
            "It holds {} kinds of record: {}. Ids read `<kind>:<id>`; filter one kind with `where _kind=<kind>`, \
             count each with `by: _kind`. Records of different kinds that share a group belong to the same entity.\n",
            kinds.len(),
            kinds.join("; ")
        ));
    }
    let largest: Vec<String> = describe
        .get("largest_groups")
        .items()
        .iter()
        .take(8)
        .map(|g| {
            let key = g.get("key").str_or("?");
            let label = g.get("name").as_str().filter(|n| *n != key).map(|n| format!("{n} ({key})")).unwrap_or(key.to_string());
            format!("{label}: {}", g.get("records").as_i64().unwrap_or(0))
        })
        .collect();
    if !largest.is_empty() {
        out.push_str(&format!("Largest {}: {}.\n", plural(group, 2), largest.join("; ")));
    }
    let filters: Vec<String> = describe
        .get("filters")
        .items()
        .iter()
        .take(8)
        .map(|f| {
            let values: Vec<&str> = f.get("top").items().iter().take(6).filter_map(|v| v.at(0).as_str()).collect();
            format!("{} ({})", f.get("field").str_or("?"), values.join(", "))
        })
        .collect();
    if !filters.is_empty() {
        out.push_str(&format!("Exact-match filters for `where`: {}.\n", filters.join("; ")));
    }
    if let Some(search) = &names.search {
        out.push_str(&format!(
            "Use `{search}` for documents, notes, and free text across every source (ranked, with record ids). \
             It ORs your words and ranks by relevance, so search with a few specific terms rather than a full sentence."
        ));
        if let Some(group) = &names.resolve {
            out.push_str(&format!(" Resolve a loose source or entity name with `{group}` first."));
        }
        if let Some(aggregate) = &names.aggregate {
            out.push_str(&format!(
                " For how many, how many different, how much, or which has the most, use `{aggregate}`: exact counts, \
                 distinct counts, sums, averages, minimums and maximums over every matching record, in total or by {group}, \
                 year, month or any field, or two at once ({group},month), and only records meeting number conditions \
                 (hours>4). Use it instead of writing a script over the data; search and aggregate answer from the index \
                 in milliseconds."
            ));
        }
        if let Some(get) = &names.get {
            out.push_str(&format!(" Open a full record with `{get}`."));
        }
        out.push_str(&format!(" {LARGE_DATA}"));
        if let Some(index) = &names.index {
            out.push_str(&format!(" Index new data the user gives you with `{index}`."));
        }
        if names.renamed {
            out.push_str(" Leviathan's results name its tools without your prefix (`search`, `get`); call them by your names.");
        }
        out.push('\n');
    }
    out.push_str("Each user message may arrive with a <workspace_recall> block: records the harness retrieved for it automatically. Use the relevant ones, search again when they are not enough, and cite record ids or source names for facts you take from the index.\n");
    out
}

pub fn render_recall(result: &Json, min_relevance: f64, names: &ToolNames) -> Option<String> {
    let cards: Vec<&Json> = result
        .get("results")
        .items()
        .iter()
        .filter(|c| c.get("relevance").as_f64().is_none_or(|r| r >= min_relevance))
        .collect();
    if cards.is_empty() {
        return None;
    }
    let total = result.get("total_matches").as_i64().unwrap_or(cards.len() as i64);
    let mut out = format!(
        "<workspace_recall source=\"leviathan\" shown=\"{}\" matches=\"{total}\">\n\
         Retrieved automatically from the workspace index for this message. It may be incomplete or partly irrelevant; \
         prefer exact queries for numbers, and cite record ids.\n",
        cards.len()
    );
    for (index, card) in cards.iter().enumerate() {
        let title = card.get("title").as_str().unwrap_or("(untitled)");
        out.push_str(&format!("{}. {title} [id: {}]", index + 1, card.get("id").str_or("?")));
        let group = card.get("group_name").as_str().or(card.get("group").as_str());
        let mut meta = Vec::new();
        if let Some(group) = group {
            meta.push(group.to_string());
        }
        if let Some(date) = card.get("date").as_str() {
            meta.push(date[..date.len().min(10)].to_string());
        }
        if !meta.is_empty() {
            out.push_str(&format!(" ({})", meta.join(", ")));
        }
        out.push('\n');
        let fields: Vec<String> = card
            .get("fields")
            .entries()
            .iter()
            .take(8)
            .map(|(k, v)| format!("{k}: {}", v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())))
            .collect();
        if !fields.is_empty() {
            out.push_str(&format!("   {}\n", util::truncate(&fields.join(" · "), 400, "…")));
        }
        if let Some(snippet) = card.get("snippet").as_str() {
            out.push_str(&format!("   {}\n", util::truncate(&snippet.split_whitespace().collect::<Vec<_>>().join(" "), 500, "…")));
        }
    }
    if total > cards.len() as i64
        && let Some(search) = &names.search
    {
        out.push_str(&format!("({} more matches; page through them with `{search}`.)\n", total - cards.len() as i64));
    }
    out.push_str("</workspace_recall>");
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;

    fn names() -> ToolNames {
        ToolNames {
            search: Some("leviathan_search".into()),
            get: Some("leviathan_get".into()),
            resolve: None,
            aggregate: Some("leviathan_aggregate".into()),
            index: None,
            renamed: true,
        }
    }

    #[test]
    fn strips_a_leading_context_line() {
        let message = "[Today is Sunday, October 25, 2026. Context from the screen the user is on: Results]\n\nWhich supplier is late?";
        assert_eq!(recall_query(message), "Which supplier is late?");
        assert_eq!(recall_query("  plain   question "), "plain question");
    }

    #[test]
    fn renders_briefing_and_recall() {
        let describe = json::parse(
            r#"{"about":{"record":"record","group":"source"},"record_count":120,"group_count":3,
                "date_min":"2026-01-02T00:00:00Z","date_max":"2026-10-01",
                "largest_groups":[{"key":"sample_plant_work_orders","records":80}],
                "filters":[{"field":"kind","distinct":2,"top":[["table_row",100],["document",20]]}]}"#,
        )
        .unwrap();
        let text = render_briefing(&describe, &names());
        assert!(text.contains("120 records across 3 sources, dated 2026-01-02 to 2026-10-01"), "{text}");
        assert!(text.contains("kind (table_row, document)"));
        assert!(text.contains("`leviathan_search`"));
        assert!(text.contains("use `leviathan_aggregate`") && text.contains("by source, year, month or any field"), "{text}");
        assert!(text.contains("without your prefix"));

        let search = json::parse(
            r#"{"total_matches":9,"results":[
                {"id":"wo:1","title":"Filler 3 jam","group":"sample_plant_work_orders","date":"2026-09-30T10:00:00Z","fields":{"status":"open"},"snippet":"bearing  failure","relevance":1.0},
                {"id":"wo:2","title":"noise","relevance":0.01}]}"#,
        )
        .unwrap();
        let recall = render_recall(&search, 0.15, &names()).unwrap();
        assert!(recall.contains("1. Filler 3 jam [id: wo:1] (sample_plant_work_orders, 2026-09-30)"), "{recall}");
        assert!(recall.contains("status: open"));
        assert!(recall.contains("bearing failure"));
        assert!(!recall.contains("noise"));
        assert!(recall.contains("8 more matches"));
        assert!(render_recall(&json::parse(r#"{"results":[]}"#).unwrap(), 0.15, &names()).is_none());
    }

    #[test]
    fn briefs_each_kind_and_survives_odd_dates() {
        let describe = json::parse(
            r#"{"about":{"record":"record","group":"group"},"record_count":5,"group_count":2,
                "date_min":"2024年3月11日 午後","date_max":"2024年3月11日 午後",
                "kinds":[{"name":"tickets","record":"ticket","group":"customer","records":3,
                          "fields":{"id":"id","group":"customer_id","date":["opened"]}},
                         {"name":"customers","record":"customer","group":"customer","records":2,
                          "fields":{"id":"id","group":"id"}}],
                "filters":[{"field":"_kind","distinct":2,"top":[["tickets",3],["customers",2]]}]}"#,
        )
        .unwrap();
        let text = render_briefing(&describe, &names());
        assert!(text.contains("dated 2024年3月11日 to 2024年3月11日"), "{text}");
        assert!(
            text.contains("2 kinds of record: tickets (3 tickets, grouped by customer_id, dated by opened); customers (2 customers, grouped by id)"),
            "{text}"
        );
        assert!(text.contains("where _kind=<kind>"));
    }

    #[test]
    fn empty_index_says_how_to_index_when_it_can() {
        let mut names = names();
        let empty = json::parse(r#"{"record_count":0,"group_count":0}"#).unwrap();
        assert!(render_briefing(&empty, &names).contains("nothing has been uploaded"));
        names.index = Some("leviathan_index".into());
        let text = render_briefing(&empty, &names);
        assert!(text.contains("No dataset is indexed yet") && text.contains("`leviathan_index`"), "{text}");
    }
}
