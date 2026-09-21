//! Time utilities tool for Syscity
//!
//! This tool provides time-related utilities like getting current time,
//! formatting dates, calculating time differences, and scheduling reminders.

use async_trait::async_trait;
use chrono::{DateTime, Local, Utc};
use serde_json::Value;
use tracing::{debug, info};

use super::{create_schema, Tool, ToolContext, ToolExecutionResult};
use crate::tools::sdk::ToolCapabilities;

/// Time tool for time-related operations
#[derive(Debug, Default)]
pub struct TimeTool;

impl TimeTool {
    /// Create a new time tool
    pub fn new() -> Self {
        Self
    }

    /// Get current time in various formats
    fn get_current_time(&self, timezone: Option<&str>) -> crate::Result<TimeInfo> {
        let now_utc = Utc::now();
        let now_local = Local::now();

        let timezone_name = match timezone {
            Some("UTC") | Some("utc") => "UTC",
            Some("local") | Some("LOCAL") | None => "Local",
            Some(tz) => {
                return Err(crate::error::SyscityError::Validation(format!(
                    "Unsupported timezone: {}. Use 'UTC' or 'local'",
                    tz
                )));
            }
        };

        Ok(TimeInfo {
            timestamp: now_utc.timestamp(),
            iso8601: now_utc.to_rfc3339(),
            date: now_local.format("%Y-%m-%d").to_string(),
            time: now_local.format("%H:%M:%S").to_string(),
            day_of_week: now_local.format("%A").to_string(),
            timezone: timezone_name.to_string(),
            unix_timestamp: now_utc.timestamp(),
        })
    }

    /// Format a date/time according to a format string
    fn format_time(&self, format: &str, timestamp: Option<i64>) -> crate::Result<String> {
        let datetime: DateTime<Utc> = match timestamp {
            Some(ts) => DateTime::from_timestamp(ts, 0).ok_or_else(|| {
                crate::error::SyscityError::Validation("Invalid timestamp".to_string())
            })?,
            None => Utc::now(),
        };

        // Map common format names to strftime format strings
        let format_str = match format {
            "iso" | "ISO" => "%Y-%m-%dT%H:%M:%S%:z",
            "rfc2822" | "RFC2822" => "%a, %d %b %Y %H:%M:%S %z",
            "rfc3339" | "RFC3339" => "%Y-%m-%dT%H:%M:%S%:z",
            "short" => "%Y-%m-%d %H:%M",
            "date" => "%Y-%m-%d",
            "time" => "%H:%M:%S",
            "long" => "%A, %B %d, %Y at %I:%M %p",
            "compact" => "%Y%m%d_%H%M%S",
            custom => custom, // Use as-is for custom formats
        };

        Ok(datetime.format(format_str).to_string())
    }

    /// Parse a natural language time expression
    fn parse_natural_time(&self, expression: &str) -> Option<TimeParseResult> {
        let expr = expression.to_lowercase();

        // Handle common expressions
        if expr.contains("now") {
            let now = Utc::now();
            return Some(TimeParseResult {
                timestamp: now.timestamp(),
                description: "now".to_string(),
            });
        }

        if expr.contains("tomorrow") {
            let tomorrow = Utc::now() + chrono::Duration::days(1);
            // Set to start of day if just "tomorrow", otherwise keep time
            let tomorrow = if expr.contains("morning") {
                tomorrow.date_naive().and_hms_opt(9, 0, 0)
            } else if expr.contains("afternoon") {
                tomorrow.date_naive().and_hms_opt(14, 0, 0)
            } else if expr.contains("evening") {
                tomorrow.date_naive().and_hms_opt(18, 0, 0)
            } else if expr.contains("night") {
                tomorrow.date_naive().and_hms_opt(20, 0, 0)
            } else {
                Some(tomorrow.naive_utc())
            };

            if let Some(naive) = tomorrow {
                if let Some(datetime) = DateTime::from_timestamp(naive.and_utc().timestamp(), 0) {
                    return Some(TimeParseResult {
                        timestamp: datetime.timestamp(),
                        description: "tomorrow".to_string(),
                    });
                }
            }
        }

        if expr.contains("in ") {
            // Parse "in X minutes/hours/days"
            if let Some(minutes) = Self::extract_duration(&expr, "minute", "minutes") {
                let future = Utc::now() + chrono::Duration::minutes(minutes);
                return Some(TimeParseResult {
                    timestamp: future.timestamp(),
                    description: format!("in {} minutes", minutes),
                });
            }
            if let Some(hours) = Self::extract_duration(&expr, "hour", "hours") {
                let future = Utc::now() + chrono::Duration::hours(hours);
                return Some(TimeParseResult {
                    timestamp: future.timestamp(),
                    description: format!("in {} hours", hours),
                });
            }
            if let Some(days) = Self::extract_duration(&expr, "day", "days") {
                let future = Utc::now() + chrono::Duration::days(days);
                return Some(TimeParseResult {
                    timestamp: future.timestamp(),
                    description: format!("in {} days", days),
                });
            }
        }

        None
    }

    /// Extract a duration value from text
    fn extract_duration(text: &str, singular: &str, plural: &str) -> Option<i64> {
        let pattern = format!("in (\\d+) {}", singular);
        let pattern_plural = format!("in (\\d+) {}", plural);

        // Try plural first, then singular
        if let Ok(re) = regex::Regex::new(&pattern_plural) {
            if let Some(caps) = re.captures(text) {
                if let Some(num) = caps.get(1) {
                    return num.as_str().parse().ok();
                }
            }
        }

        if let Ok(re) = regex::Regex::new(&pattern) {
            if let Some(caps) = re.captures(text) {
                if let Some(num) = caps.get(1) {
                    return num.as_str().parse().ok();
                }
            }
        }

        None
    }

    /// Calculate time difference between two timestamps
    fn time_diff(&self, from: i64, to: i64) -> TimeDifference {
        let diff = to - from;
        let abs_diff = diff.abs();

        let days = abs_diff / 86400;
        let hours = (abs_diff % 86400) / 3600;
        let minutes = (abs_diff % 3600) / 60;
        let seconds = abs_diff % 60;

        TimeDifference {
            total_seconds: abs_diff,
            days,
            hours,
            minutes,
            seconds,
            direction: if diff >= 0 { "future" } else { "past" }.to_string(),
            human_readable: if days > 0 {
                format!("{} days, {} hours", days, hours)
            } else if hours > 0 {
                format!("{} hours, {} minutes", hours, minutes)
            } else if minutes > 0 {
                format!("{} minutes, {} seconds", minutes, seconds)
            } else {
                format!("{} seconds", seconds)
            },
        }
    }
}

/// Time information structure
#[derive(Debug, Clone, serde::Serialize)]
struct TimeInfo {
    timestamp: i64,
    iso8601: String,
    date: String,
    time: String,
    day_of_week: String,
    timezone: String,
    unix_timestamp: i64,
}

/// Time parse result
#[derive(Debug, Clone)]
struct TimeParseResult {
    timestamp: i64,
    description: String,
}

/// Time difference structure
#[derive(Debug, Clone, serde::Serialize)]
struct TimeDifference {
    total_seconds: i64,
    days: i64,
    hours: i64,
    minutes: i64,
    seconds: i64,
    direction: String,
    human_readable: String,
}

#[async_trait]
impl Tool for TimeTool {
    fn name(&self) -> &str {
        "time"
    }

    fn description(&self) -> &str {
        r#"Time utilities: get current time, format dates, parse natural language time expressions.

Use this for:
- Getting the current date/time
- Converting timestamps to readable formats
- Understanding time expressions like "tomorrow morning" or "in 30 minutes"
- Calculating time differences"#
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Time utility operations",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "enum": ["now", "format", "parse", "diff"],
                    "description": "The time operation to perform"
                },
                "timezone": {
                    "type": "string",
                    "description": "Timezone for 'now' action ('UTC' or 'local')",
                    "default": "local"
                },
                "format": {
                    "type": "string",
                    "description": "Format string for 'format' action (e.g., 'iso', 'short', 'long', or custom strftime format)"
                },
                "timestamp": {
                    "type": "integer",
                    "description": "Unix timestamp for 'format' or 'diff' actions"
                },
                "expression": {
                    "type": "string",
                    "description": "Natural language time expression for 'parse' action"
                },
                "from": {
                    "type": "integer",
                    "description": "Starting timestamp for 'diff' action"
                },
                "to": {
                    "type": "integer",
                    "description": "Ending timestamp for 'diff' action (defaults to now)"
                }
            }),
            vec!["action"],
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            read_only: true,
            requires_approval: false,
            risk_level: crate::tools::approval::RiskLevel::Low,
            categories: vec!["system".to_string(), "info".to_string()],
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let action = args["action"].as_str().ok_or_else(|| {
            crate::error::SyscityError::Validation("Missing 'action' argument".to_string())
        })?;

        match action {
            "now" => {
                let timezone = args["timezone"].as_str();
                let info = self.get_current_time(timezone)?;

                debug!("Getting current time: {:?}", info);

                Ok(ToolExecutionResult::success(format!(
                    "Current time ({}): {} {}\nISO8601: {}\nDay: {}",
                    info.timezone, info.date, info.time, info.iso8601, info.day_of_week
                ))
                .with_data(serde_json::json!(info)))
            }

            "format" => {
                let format = args["format"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation("Missing 'format' argument".to_string())
                })?;
                let timestamp = args["timestamp"].as_i64();

                let formatted = self.format_time(format, timestamp)?;
                let source = if timestamp.is_some() {
                    "provided timestamp"
                } else {
                    "current time"
                };

                info!("Formatted time: {} (format: {})", formatted, format);

                Ok(ToolExecutionResult::success(formatted.clone()).with_data(serde_json::json!({
                    "formatted": formatted,
                    "format": format,
                    "source": source
                })))
            }

            "parse" => {
                let expression = args["expression"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "Missing 'expression' argument".to_string(),
                    )
                })?;

                match self.parse_natural_time(expression) {
                    Some(result) => {
                        let datetime = DateTime::from_timestamp(result.timestamp, 0)
                            .map(|d| d.to_rfc3339())
                            .unwrap_or_default();

                        Ok(ToolExecutionResult::success(format!(
                            "Parsed '{}' as: {} ({})",
                            expression, datetime, result.description
                        ))
                        .with_data(serde_json::json!({
                            "timestamp": result.timestamp,
                            "iso8601": datetime,
                            "description": result.description
                        })))
                    }
                    None => Ok(ToolExecutionResult::error(format!(
                        "Could not parse time expression: '{}'",
                        expression
                    ))),
                }
            }

            "diff" => {
                let from = args["from"].as_i64().ok_or_else(|| {
                    crate::error::SyscityError::Validation("Missing 'from' timestamp".to_string())
                })?;
                let to = args["to"]
                    .as_i64()
                    .unwrap_or_else(|| Utc::now().timestamp());

                let diff = self.time_diff(from, to);

                Ok(ToolExecutionResult::success(format!(
                    "Time difference: {} ({})",
                    diff.human_readable, diff.direction
                ))
                .with_data(serde_json::json!(diff)))
            }

            _ => Err(crate::error::SyscityError::Validation(format!(
                "Unknown action: {}. Valid actions: now, format, parse, diff",
                action
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_time_tool_creation() {
        let tool = TimeTool::new();
        assert_eq!(tool.name(), "time");
    }

    #[test]
    fn test_get_current_time() {
        let tool = TimeTool::new();

        let utc_info = tool.get_current_time(Some("UTC")).unwrap();
        assert_eq!(utc_info.timezone, "UTC");

        let local_info = tool.get_current_time(None).unwrap();
        assert_eq!(local_info.timezone, "Local");
    }

    #[test]
    fn test_format_time() {
        let tool = TimeTool::new();

        // Test with a known timestamp (2024-01-01 00:00:00 UTC)
        let ts = 1704067200i64;

        let iso = tool.format_time("iso", Some(ts)).unwrap();
        assert!(iso.contains("2024"));

        let date = tool.format_time("date", Some(ts)).unwrap();
        assert_eq!(date, "2024-01-01");

        let custom = tool.format_time("%Y/%m/%d", Some(ts)).unwrap();
        assert_eq!(custom, "2024/01/01");
    }

    #[test]
    fn test_parse_natural_time() {
        let tool = TimeTool::new();

        let now_result = tool.parse_natural_time("now").unwrap();
        assert!(now_result.timestamp > 0);

        let tomorrow_result = tool.parse_natural_time("tomorrow").unwrap();
        let now = Utc::now().timestamp();
        assert!(tomorrow_result.timestamp > now);
    }

    #[test]
    fn test_time_diff() {
        let tool = TimeTool::new();

        let from = 0i64; // 1970-01-01 00:00:00 UTC
        let to = 3600i64; // 1 hour later

        let diff = tool.time_diff(from, to);
        assert_eq!(diff.hours, 1);
        assert_eq!(diff.minutes, 0);
        assert_eq!(diff.direction, "future");

        let diff_reverse = tool.time_diff(to, from);
        assert_eq!(diff_reverse.direction, "past");
    }
}
