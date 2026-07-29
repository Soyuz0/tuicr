use crate::error::{Result, TuicrError};
use crate::forge::traits::{ForgeRepository, PullRequestTarget};

const DEV_AZURE_HOST: &str = "dev.azure.com";
const SSH_AZURE_HOST: &str = "ssh.dev.azure.com";

/// Parse one of Azure DevOps' documented Git remote URL forms.
pub fn parse_azure_devops_remote_url(remote_url: &str) -> Option<ForgeRepository> {
    let original = trim_url_suffix(remote_url.trim());
    if original.is_empty() {
        return None;
    }

    if let Some(path) = original.strip_prefix("git@ssh.dev.azure.com:") {
        return repository_from_ssh_path(path);
    }

    if let Some(rest) = strip_prefix_ignore_ascii_case(original, "ssh://") {
        let (authority, path) = rest.split_once('/')?;
        let (user, host) = authority.rsplit_once('@')?;
        if user != "git" || !host.eq_ignore_ascii_case(SSH_AZURE_HOST) {
            return None;
        }
        return repository_from_ssh_path(path);
    }

    let rest = strip_prefix_ignore_ascii_case(original, "https://")?;
    let (authority, path) = rest.split_once('/')?;
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if host.eq_ignore_ascii_case(DEV_AZURE_HOST) {
        repository_from_dev_azure_path(path)
    } else if let Some(organization) = visual_studio_organization(host) {
        repository_from_visual_studio_path(host, organization, path)
    } else {
        None
    }
}

/// Parse a positive PR number or a full Azure DevOps URL ending in
/// `/_git/<repository>/pullrequest/<number>`.
pub fn parse_pull_request_target_azure_devops(input: &str) -> Result<PullRequestTarget> {
    let trimmed = input.trim();
    if let Some(number) = parse_positive_number(trimmed) {
        return Ok(PullRequestTarget::number(number, trimmed));
    }

    let without_suffix = trim_url_suffix(trimmed);
    let (repository_url, number) = without_suffix
        .rsplit_once("/pullrequest/")
        .and_then(|(repository_url, number)| Some((repository_url, parse_positive_number(number)?)))
        .ok_or_else(|| malformed_target(input))?;
    let repository =
        parse_azure_devops_remote_url(repository_url).ok_or_else(|| malformed_target(input))?;

    Ok(PullRequestTarget::with_repository(
        repository, number, trimmed,
    ))
}

/// RFC 3986 percent-encode a single path component. `/` is encoded so the
/// result can safely be embedded in stable Azure repository coordinates.
pub(crate) fn percent_encode_path_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(hex_digit(byte >> 4));
            encoded.push(hex_digit(byte & 0x0f));
        }
    }
    encoded
}

/// Decode one URL path component, rejecting malformed escapes and invalid
/// UTF-8 rather than silently changing repository identity.
pub(crate) fn percent_decode_path_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = *bytes.get(index + 1)?;
            let low = *bytes.get(index + 2)?;
            decoded.push((hex_value(high)? << 4) | hex_value(low)?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn repository_from_dev_azure_path(path: &str) -> Option<ForgeRepository> {
    let parts = decode_path(path)?;
    let [organization, project, git_segment, repository] = parts.as_slice() else {
        return None;
    };
    if git_segment != "_git" {
        return None;
    }
    build_repository(
        DEV_AZURE_HOST,
        organization,
        format!(
            "https://{DEV_AZURE_HOST}/{}",
            percent_encode_path_component(organization)
        ),
        project,
        repository,
    )
}

fn repository_from_visual_studio_path(
    host: &str,
    organization: &str,
    path: &str,
) -> Option<ForgeRepository> {
    let parts = decode_path(path)?;
    let [project, git_segment, repository] = parts.as_slice() else {
        return None;
    };
    if git_segment != "_git" {
        return None;
    }
    build_repository(
        &host.to_ascii_lowercase(),
        organization,
        format!("https://{}", host.to_ascii_lowercase()),
        project,
        repository,
    )
}

fn repository_from_ssh_path(path: &str) -> Option<ForgeRepository> {
    let parts = decode_path(path)?;
    let [version, organization, project, repository] = parts.as_slice() else {
        return None;
    };
    if version != "v3" {
        return None;
    }
    build_repository(
        DEV_AZURE_HOST,
        organization,
        format!(
            "https://{DEV_AZURE_HOST}/{}",
            percent_encode_path_component(organization)
        ),
        project,
        repository,
    )
}

fn build_repository(
    host: &str,
    organization: &str,
    organization_url: String,
    project: &str,
    repository: &str,
) -> Option<ForgeRepository> {
    let repository = repository.strip_suffix(".git").unwrap_or(repository);
    if [organization, project, repository]
        .iter()
        .any(|component| component.is_empty())
    {
        return None;
    }
    Some(ForgeRepository::azure_devops(
        host,
        organization,
        organization_url,
        project,
        repository,
    ))
}

fn decode_path(path: &str) -> Option<Vec<String>> {
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return None;
    }
    path.split('/')
        .map(|part| {
            if part.is_empty() {
                None
            } else {
                percent_decode_path_component(part)
            }
        })
        .collect()
}

fn visual_studio_organization(host: &str) -> Option<&str> {
    let suffix = ".visualstudio.com";
    if host.len() <= suffix.len()
        || !host
            .get(host.len() - suffix.len()..)?
            .eq_ignore_ascii_case(suffix)
    {
        return None;
    }
    let organization = &host[..host.len() - suffix.len()];
    if organization.is_empty() || organization.contains('.') {
        None
    } else {
        Some(organization)
    }
}

fn parse_positive_number(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse::<u64>().ok().filter(|number| *number > 0)
}

fn trim_url_suffix(value: &str) -> &str {
    value
        .split(['?', '#'])
        .next()
        .unwrap_or(value)
        .trim_end_matches('/')
}

fn strip_prefix_ignore_ascii_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let candidate = value.get(..prefix.len())?;
    if candidate.eq_ignore_ascii_case(prefix) {
        value.get(prefix.len()..)
    } else {
        None
    }
}

fn malformed_target(input: &str) -> TuicrError {
    TuicrError::Forge(format!(
        "Malformed Azure DevOps pull request target: `{input}`"
    ))
}

fn hex_digit(value: u8) -> char {
    char::from(if value < 10 {
        b'0' + value
    } else {
        b'A' + value - 10
    })
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_repository(
        input: &str,
        host: &str,
        organization: &str,
        organization_url: &str,
        project: &str,
        name: &str,
    ) {
        let repository = parse_azure_devops_remote_url(input).expect("valid Azure remote");
        let identity = repository
            .azure_devops_identity()
            .expect("Azure repository identity");
        assert_eq!(repository.host, host);
        assert_eq!(repository.name, name);
        assert_eq!(identity.organization, organization);
        assert_eq!(identity.organization_url, organization_url);
        assert_eq!(identity.project, project);
    }

    #[test]
    fn parses_every_documented_remote_form() {
        let cases = [
            "https://dev.azure.com/acme/widgets/_git/service",
            "https://acme@dev.azure.com/acme/widgets/_git/service",
            "git@ssh.dev.azure.com:v3/acme/widgets/service",
            "ssh://git@ssh.dev.azure.com/v3/acme/widgets/service",
        ];
        for input in cases {
            assert_repository(
                input,
                "dev.azure.com",
                "acme",
                "https://dev.azure.com/acme",
                "widgets",
                "service",
            );
        }
        assert_repository(
            "https://acme.visualstudio.com/widgets/_git/service",
            "acme.visualstudio.com",
            "acme",
            "https://acme.visualstudio.com",
            "widgets",
            "service",
        );
    }

    #[test]
    fn decodes_components_and_only_strips_repository_git_suffix() {
        assert_repository(
            "https://dev.azure.com/my%20org/project.git/_git/repo%20name.git/?ignored=yes#x",
            "dev.azure.com",
            "my org",
            "https://dev.azure.com/my%20org",
            "project.git",
            "repo name",
        );
    }

    #[test]
    fn rejects_malformed_or_ambiguous_remotes() {
        let invalid = [
            "",
            "http://dev.azure.com/org/project/_git/repo",
            "https://dev.azure.com/org/project/repo",
            "https://dev.azure.com/org/project/_git/repo/extra",
            "https://dev.azure.com/org/project/_git/bad%2",
            "https://dev.azure.com/org/project/_git/bad%ZZ",
            "git@ssh.dev.azure.com:v2/org/project/repo",
            "ssh://other@ssh.dev.azure.com/v3/org/project/repo",
            "https://sub.org.visualstudio.com/project/_git/repo",
        ];
        for input in invalid {
            assert!(
                parse_azure_devops_remote_url(input).is_none(),
                "accepted {input}"
            );
        }
    }

    #[test]
    fn parses_numeric_and_full_url_pull_request_targets() {
        let numeric = parse_pull_request_target_azure_devops(" 42 ").unwrap();
        assert_eq!(numeric.number, 42);
        assert!(numeric.repository.is_none());

        let input = "https://dev.azure.com/my%20org/my%20project/_git/my%20repo/pullrequest/17/?x=1#discussion";
        let target = parse_pull_request_target_azure_devops(input).unwrap();
        assert_eq!(target.number, 17);
        let repository = target.repository.unwrap();
        assert_eq!(repository.name, "my repo");
        assert_eq!(
            repository.azure_devops_identity().unwrap().project,
            "my project"
        );
    }

    #[test]
    fn rejects_invalid_pull_request_targets() {
        for input in [
            "",
            "0",
            "-1",
            "https://dev.azure.com/org/project/_git/repo/pullrequest/0",
            "https://dev.azure.com/org/project/_git/repo/pullrequest/nope",
            "https://dev.azure.com/org/project/_git/repo/pullrequest/1/extra",
        ] {
            assert!(parse_pull_request_target_azure_devops(input).is_err());
        }
    }

    #[test]
    fn percent_codec_round_trips_utf8_and_reserved_bytes() {
        let value = "space / hash # percent % caf\u{e9}";
        let encoded = percent_encode_path_component(value);
        assert_eq!(
            percent_decode_path_component(&encoded).as_deref(),
            Some(value)
        );
        assert!(percent_decode_path_component("%FF").is_none());
        assert!(percent_decode_path_component("%").is_none());
    }
}
