use anyhow::{anyhow, bail, Result};
use k8s_gateway_api as api;
use kube::{Resource, ResourceExt};
use linkerd_policy_controller_core::routes::{self, GroupKindName, GroupKindNamespaceName};
use linkerd_policy_controller_k8s_api::policy;
use std::num::NonZeroU16;

#[derive(Debug, Clone)]
pub(crate) enum HttpRouteResource {
    Linkerd(linkerd_policy_controller_k8s_api::policy::HttpRoute),
    Gateway(api::HttpRoute),
}

impl HttpRouteResource {
    pub(crate) fn name(&self) -> String {
        match self {
            HttpRouteResource::Linkerd(route) => route.name_unchecked(),
            HttpRouteResource::Gateway(route) => route.name_unchecked(),
        }
    }

    pub(crate) fn namespace(&self) -> String {
        match self {
            HttpRouteResource::Linkerd(route) => {
                route.namespace().expect("HttpRoute must have a namespace")
            }
            HttpRouteResource::Gateway(route) => {
                route.namespace().expect("HttpRoute must have a namespace")
            }
        }
    }

    pub(crate) fn inner(&self) -> &api::CommonRouteSpec {
        match self {
            HttpRouteResource::Linkerd(route) => &route.spec.inner,
            HttpRouteResource::Gateway(route) => &route.spec.inner,
        }
    }

    pub(crate) fn status(&self) -> Option<&api::RouteStatus> {
        match self {
            HttpRouteResource::Linkerd(route) => route.status.as_ref().map(|status| &status.inner),
            HttpRouteResource::Gateway(route) => route.status.as_ref().map(|status| &status.inner),
        }
    }

    pub(crate) fn gknn(&self) -> GroupKindNamespaceName {
        match self {
            HttpRouteResource::Linkerd(route) => gkn_for_resource(route)
                .namespaced(route.namespace().expect("Route must have namespace")),
            HttpRouteResource::Gateway(route) => gkn_for_resource(route)
                .namespaced(route.namespace().expect("Route must have namespace")),
        }
    }
}

pub fn try_match(
    api::HttpRouteMatch {
        path,
        headers,
        query_params,
        method,
    }: api::HttpRouteMatch,
) -> Result<routes::HttpRouteMatch> {
    let path = path.map(path_match).transpose()?;

    let headers = headers
        .into_iter()
        .flatten()
        .map(header_match)
        .collect::<Result<_>>()?;

    let query_params = query_params
        .into_iter()
        .flatten()
        .map(query_param_match)
        .collect::<Result<_>>()?;

    let method = method
        .as_deref()
        .map(routes::Method::try_from)
        .transpose()?;

    Ok(routes::HttpRouteMatch {
        path,
        headers,
        query_params,
        method,
    })
}

pub fn path_match(path_match: api::HttpPathMatch) -> Result<routes::PathMatch> {
    match path_match {
            api::HttpPathMatch::Exact { value } | api::HttpPathMatch::PathPrefix { value }
                if !value.starts_with('/') =>
            {
                Err(anyhow!("HttpPathMatch paths must be absolute (begin with `/`); {value:?} is not an absolute path"))
            }
            api::HttpPathMatch::Exact { value } => Ok(routes::PathMatch::Exact(value)),
            api::HttpPathMatch::PathPrefix { value } => Ok(routes::PathMatch::Prefix(value)),
            api::HttpPathMatch::RegularExpression { value } => value
                .parse()
                .map(routes::PathMatch::Regex)
                .map_err(Into::into),
        }
}

pub fn host_match(hostname: api::Hostname) -> routes::HostMatch {
    if hostname.starts_with("*.") {
        let mut reverse_labels = hostname
            .split('.')
            .skip(1)
            .map(|label| label.to_string())
            .collect::<Vec<String>>();
        reverse_labels.reverse();
        routes::HostMatch::Suffix { reverse_labels }
    } else {
        routes::HostMatch::Exact(hostname)
    }
}

pub fn header_match(header_match: api::HttpHeaderMatch) -> Result<routes::HeaderMatch> {
    match header_match {
        api::HttpHeaderMatch::Exact { name, value } => {
            Ok(routes::HeaderMatch::Exact(name.parse()?, value.parse()?))
        }
        api::HttpHeaderMatch::RegularExpression { name, value } => {
            Ok(routes::HeaderMatch::Regex(name.parse()?, value.parse()?))
        }
    }
}

pub fn query_param_match(query_match: api::HttpQueryParamMatch) -> Result<routes::QueryParamMatch> {
    match query_match {
        api::HttpQueryParamMatch::Exact { name, value } => {
            Ok(routes::QueryParamMatch::Exact(name, value))
        }
        api::HttpQueryParamMatch::RegularExpression { name, value } => {
            Ok(routes::QueryParamMatch::Regex(name, value.parse()?))
        }
    }
}

pub fn header_modifier(
    api::HttpRequestHeaderFilter { set, add, remove }: api::HttpRequestHeaderFilter,
) -> Result<routes::HeaderModifierFilter> {
    Ok(routes::HeaderModifierFilter {
        add: add
            .into_iter()
            .flatten()
            .map(|api::HttpHeader { name, value }| Ok((name.parse()?, value.parse()?)))
            .collect::<Result<Vec<_>>>()?,
        set: set
            .into_iter()
            .flatten()
            .map(|api::HttpHeader { name, value }| Ok((name.parse()?, value.parse()?)))
            .collect::<Result<Vec<_>>>()?,
        remove: remove
            .into_iter()
            .flatten()
            .map(routes::HeaderName::try_from)
            .collect::<Result<_, _>>()?,
    })
}

pub fn req_redirect(
    api::HttpRequestRedirectFilter {
        scheme,
        hostname,
        path,
        port,
        status_code,
    }: api::HttpRequestRedirectFilter,
) -> Result<routes::RequestRedirectFilter> {
    Ok(routes::RequestRedirectFilter {
        scheme: scheme.as_deref().map(TryInto::try_into).transpose()?,
        host: hostname,
        path: path.map(path_modifier).transpose()?,
        port: port.and_then(|p| NonZeroU16::try_from(p).ok()),
        status: status_code.map(routes::StatusCode::try_from).transpose()?,
    })
}

fn path_modifier(path_modifier: api::HttpPathModifier) -> Result<routes::PathModifier> {
    use api::HttpPathModifier::*;
    match path_modifier {
        ReplaceFullPath {
            replace_full_path: path,
        }
        | ReplacePrefixMatch {
            replace_prefix_match: path,
        } if !path.starts_with('/') => {
            bail!(
                "RequestRedirect filters may only contain absolute paths \
                    (starting with '/'); {path:?} is not an absolute path"
            )
        }
        ReplaceFullPath { replace_full_path } => Ok(routes::PathModifier::Full(replace_full_path)),
        ReplacePrefixMatch {
            replace_prefix_match,
        } => Ok(routes::PathModifier::Prefix(replace_prefix_match)),
    }
}

pub(crate) fn gkn_for_resource<T>(t: &T) -> GroupKindName
where
    T: kube::Resource<DynamicType = ()>,
{
    let kind = T::kind(&());
    let group = T::group(&());
    let name = t.name_unchecked().into();
    GroupKindName { group, kind, name }
}

pub(crate) fn gkn_for_linkerd_http_route(name: String) -> GroupKindName {
    GroupKindName {
        group: policy::HttpRoute::group(&()),
        kind: policy::HttpRoute::kind(&()),
        name: name.into(),
    }
}

pub(crate) fn gkn_for_gateway_http_route(name: String) -> GroupKindName {
    GroupKindName {
        group: api::HttpRoute::group(&()),
        kind: api::HttpRoute::kind(&()),
        name: name.into(),
    }
}
