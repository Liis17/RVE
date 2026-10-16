use engine_project::ProjectRequirements;
use engine_render::DeviceReport;

pub(crate) fn hardware_issues(
    reports: Result<&[DeviceReport], &str>,
    total_ram_mb: u64,
    logical_cpus: usize,
    requirements: &ProjectRequirements,
) -> Vec<String> {
    let mut issues = Vec::new();
    match reports {
        Err(error) => issues.push(format!("не удалось проверить Vulkan: {error}")),
        Ok(reports) if !reports.iter().any(|report| report.suitable) => {
            if reports.is_empty() {
                issues.push("Vulkan не обнаружил физических устройств".to_owned());
            } else {
                issues.push("нет подходящего Vulkan-устройства".to_owned());
                for report in reports {
                    if !report.suitable {
                        issues.push(format!("{}: {}", report.name, report.reasons.join(", ")));
                    }
                }
            }
        }
        Ok(_) => {}
    }
    if total_ram_mb < requirements.min_ram_mb {
        issues.push(format!(
            "недостаточно RAM: доступно {total_ram_mb} МиБ, требуется {} МиБ",
            requirements.min_ram_mb
        ));
    }
    if logical_cpus < requirements.min_cpu_threads as usize {
        issues.push(format!(
            "недостаточно логических CPU: доступно {logical_cpus}, требуется {}",
            requirements.min_cpu_threads
        ));
    }
    issues
}

#[cfg(test)]
mod tests {
    use super::hardware_issues;
    use engine_project::ProjectRequirements;
    use engine_render::DeviceReport;

    fn suitable() -> DeviceReport {
        DeviceReport {
            name: "GPU".to_owned(),
            vendor_id: 1,
            api_version: 1,
            discrete: true,
            suitable: true,
            reasons: Vec::new(),
        }
    }

    #[test]
    fn accepts_hardware_at_project_minimums() {
        assert!(
            hardware_issues(
                Ok(&[suitable()]),
                1024,
                4,
                &ProjectRequirements {
                    min_ram_mb: 1024,
                    min_cpu_threads: 4
                }
            )
            .is_empty()
        );
    }

    #[test]
    fn reports_probe_gpu_ram_and_cpu_failures_with_details() {
        let reports = [DeviceReport {
            name: "Old GPU".to_owned(),
            vendor_id: 0,
            api_version: 0,
            discrete: false,
            suitable: false,
            reasons: vec!["Vulkan 1.3 is required".to_owned()],
        }];
        let requirements = ProjectRequirements {
            min_ram_mb: 8192,
            min_cpu_threads: 8,
        };
        let issues = hardware_issues(Ok(&reports), 512, 2, &requirements);
        assert!(issues.iter().any(|issue| issue.contains("Old GPU")));
        assert!(issues.iter().any(|issue| issue.contains("512 МиБ")));
        assert!(issues.iter().any(|issue| issue.contains("доступно 2")));
        assert!(
            hardware_issues(Err("loader missing"), 0, 0, &requirements)
                .iter()
                .any(|issue| issue.contains("loader missing"))
        );
        assert!(
            hardware_issues(Ok(&[]), 0, 0, &requirements)
                .iter()
                .any(|issue| issue.contains("устройств"))
        );
    }
}
