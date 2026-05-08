//! PDF/Report generation — automated site reports with measurements.

use serde::{Deserialize, Serialize};

/// Report template type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReportTemplate {
    SiteOverview,
    ChangeDetection,
    ClashReport,
    ComplianceAudit,
    ProgressReport,
    Custom { template_id: String },
}

/// A section in the generated report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportSection {
    pub title: String,
    pub content: SectionContent,
    pub page_break_before: bool,
}

/// Content types for report sections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SectionContent {
    Text(String),
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Image {
        url: String,
        caption: String,
        width_percent: u32,
    },
    Chart {
        chart_type: String,
        data_json: String,
    },
    KeyValueList(Vec<(String, String)>),
    Heading(String),
}

/// Report metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportMetadata {
    pub title: String,
    pub subtitle: Option<String>,
    pub author: String,
    pub organization: String,
    pub date: String,
    pub project_name: String,
    pub logo_url: Option<String>,
    pub confidential: bool,
}

/// A complete report definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub id: String,
    pub template: ReportTemplate,
    pub metadata: ReportMetadata,
    pub sections: Vec<ReportSection>,
    pub footer_text: String,
}

/// Generate HTML report content.
pub fn generate_html_report(report: &Report) -> String {
    let mut html = String::new();
    html.push_str("<!DOCTYPE html>\n<html><head>\n");
    html.push_str("<meta charset=\"utf-8\">\n");
    html.push_str(&format!("<title>{}</title>\n", report.metadata.title));
    html.push_str("<style>\n");
    html.push_str("body { font-family: Arial, sans-serif; margin: 40px; }\n");
    html.push_str("h1 { color: #2563eb; }\n");
    html.push_str("table { border-collapse: collapse; width: 100%; }\n");
    html.push_str("th, td { border: 1px solid #ddd; padding: 8px; text-align: left; }\n");
    html.push_str("th { background-color: #f0f4f8; }\n");
    html.push_str(".confidential { color: red; font-weight: bold; }\n");
    html.push_str(".footer { margin-top: 40px; border-top: 1px solid #ccc; padding-top: 10px; font-size: 0.9em; color: #666; }\n");
    html.push_str("</style>\n</head><body>\n");

    // Header
    if let Some(ref logo) = report.metadata.logo_url {
        html.push_str(&format!("<img src=\"{}\" height=\"60\" />\n", logo));
    }
    html.push_str(&format!("<h1>{}</h1>\n", report.metadata.title));
    if let Some(ref subtitle) = report.metadata.subtitle {
        html.push_str(&format!("<h2>{}</h2>\n", subtitle));
    }
    html.push_str(&format!(
        "<p><strong>Author:</strong> {} | <strong>Date:</strong> {} | <strong>Project:</strong> {}</p>\n",
        report.metadata.author, report.metadata.date, report.metadata.project_name
    ));
    if report.metadata.confidential {
        html.push_str("<p class=\"confidential\">CONFIDENTIAL</p>\n");
    }
    html.push_str("<hr>\n");

    // Sections
    for section in &report.sections {
        if section.page_break_before {
            html.push_str("<div style=\"page-break-before: always;\"></div>\n");
        }
        html.push_str(&format!("<h3>{}</h3>\n", section.title));
        match &section.content {
            SectionContent::Text(text) => {
                html.push_str(&format!("<p>{}</p>\n", text));
            }
            SectionContent::Table { headers, rows } => {
                html.push_str("<table><thead><tr>\n");
                for h in headers {
                    html.push_str(&format!("<th>{}</th>", h));
                }
                html.push_str("</tr></thead><tbody>\n");
                for row in rows {
                    html.push_str("<tr>");
                    for cell in row {
                        html.push_str(&format!("<td>{}</td>", cell));
                    }
                    html.push_str("</tr>\n");
                }
                html.push_str("</tbody></table>\n");
            }
            SectionContent::Image {
                url,
                caption,
                width_percent,
            } => {
                html.push_str(&format!(
                    "<figure><img src=\"{}\" style=\"width: {}%\" /><figcaption>{}</figcaption></figure>\n",
                    url, width_percent, caption
                ));
            }
            SectionContent::KeyValueList(items) => {
                html.push_str("<dl>\n");
                for (key, value) in items {
                    html.push_str(&format!(
                        "<dt><strong>{}</strong></dt><dd>{}</dd>\n",
                        key, value
                    ));
                }
                html.push_str("</dl>\n");
            }
            SectionContent::Heading(text) => {
                html.push_str(&format!("<h4>{}</h4>\n", text));
            }
            SectionContent::Chart {
                chart_type,
                data_json,
            } => {
                html.push_str(&format!(
                    "<div class=\"chart\" data-type=\"{}\" data-config='{}'>[Chart: {}]</div>\n",
                    chart_type, data_json, chart_type
                ));
            }
        }
    }

    // Footer
    html.push_str(&format!(
        "<div class=\"footer\">{}</div>\n",
        report.footer_text
    ));
    html.push_str("</body></html>");
    html
}

/// Generate a site overview report from project data.
pub fn generate_site_report(
    project_name: &str,
    author: &str,
    tileset_count: usize,
    total_points: u64,
    area_sq_km: f64,
    issues: &[(String, String)], // (severity, description)
) -> Report {
    let mut sections = vec![ReportSection {
        title: "Project Summary".into(),
        content: SectionContent::KeyValueList(vec![
            ("Project".into(), project_name.into()),
            ("Total Tilesets".into(), tileset_count.to_string()),
            ("Total Points".into(), format!("{}", total_points)),
            ("Coverage Area".into(), format!("{:.2} km²", area_sq_km)),
        ]),
        page_break_before: false,
    }];

    if !issues.is_empty() {
        let rows: Vec<Vec<String>> = issues
            .iter()
            .map(|(sev, desc)| vec![sev.clone(), desc.clone()])
            .collect();
        sections.push(ReportSection {
            title: "Issues & Findings".into(),
            content: SectionContent::Table {
                headers: vec!["Severity".into(), "Description".into()],
                rows,
            },
            page_break_before: false,
        });
    }

    Report {
        id: uuid::Uuid::new_v4().to_string(),
        template: ReportTemplate::SiteOverview,
        metadata: ReportMetadata {
            title: format!("Site Report: {}", project_name),
            subtitle: None,
            author: author.into(),
            organization: String::new(),
            date: chrono::Utc::now().format("%Y-%m-%d").to_string(),
            project_name: project_name.into(),
            logo_url: None,
            confidential: false,
        },
        sections,
        footer_text: "Generated by TileTopia".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_html_report() {
        let report = Report {
            id: "r1".into(),
            template: ReportTemplate::SiteOverview,
            metadata: ReportMetadata {
                title: "Test Report".into(),
                subtitle: Some("Phase 1".into()),
                author: "Engineer".into(),
                organization: "ACME".into(),
                date: "2024-01-01".into(),
                project_name: "Project X".into(),
                logo_url: None,
                confidential: false,
            },
            sections: vec![ReportSection {
                title: "Summary".into(),
                content: SectionContent::Text("All systems operational.".into()),
                page_break_before: false,
            }],
            footer_text: "Page 1".into(),
        };
        let html = generate_html_report(&report);
        assert!(html.contains("<title>Test Report</title>"));
        assert!(html.contains("All systems operational"));
        assert!(html.contains("Phase 1"));
    }

    #[test]
    fn test_table_section() {
        let report = Report {
            id: "r2".into(),
            template: ReportTemplate::ClashReport,
            metadata: ReportMetadata {
                title: "Clash Report".into(),
                subtitle: None,
                author: "BIM Manager".into(),
                organization: "".into(),
                date: "2024-06-15".into(),
                project_name: "Building A".into(),
                logo_url: None,
                confidential: true,
            },
            sections: vec![ReportSection {
                title: "Clashes".into(),
                content: SectionContent::Table {
                    headers: vec!["ID".into(), "Type".into(), "Severity".into()],
                    rows: vec![vec!["1".into(), "Hard".into(), "Critical".into()]],
                },
                page_break_before: false,
            }],
            footer_text: "".into(),
        };
        let html = generate_html_report(&report);
        assert!(html.contains("CONFIDENTIAL"));
        assert!(html.contains("<th>Severity</th>"));
    }

    #[test]
    fn test_generate_site_report() {
        let report = generate_site_report(
            "Downtown Survey",
            "Surveyor",
            5,
            1_000_000,
            2.5,
            &[("High".into(), "Missing coverage in sector 3".into())],
        );
        assert_eq!(report.template, ReportTemplate::SiteOverview);
        assert_eq!(report.sections.len(), 2); // Summary + Issues
    }

    #[test]
    fn test_empty_issues_report() {
        let report = generate_site_report("Small Project", "Author", 1, 100, 0.1, &[]);
        assert_eq!(report.sections.len(), 1); // Only summary, no issues
    }

    #[test]
    fn test_confidential_marking() {
        let report = Report {
            id: "r3".into(),
            template: ReportTemplate::ComplianceAudit,
            metadata: ReportMetadata {
                title: "Audit".into(),
                subtitle: None,
                author: "".into(),
                organization: "".into(),
                date: "".into(),
                project_name: "".into(),
                logo_url: None,
                confidential: true,
            },
            sections: vec![],
            footer_text: "".into(),
        };
        let html = generate_html_report(&report);
        assert!(html.contains("CONFIDENTIAL"));
    }
}
