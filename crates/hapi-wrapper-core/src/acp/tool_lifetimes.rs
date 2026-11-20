//! HAPI ids per agent tool call lifetime (TS `toolCallLifetimes.ts`).
//! In memory only: the TS lifetime store that keeps open calls across a
//! wrapper restart is not ported.
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolCallEventKind {
    Start,
    Update,
    Terminal,
    Request,
}

/// Scope of the prompted ACP session; `child:<id>` for an OpenCode child.
pub const MAIN_SCOPE: &str = "main";
pub const MAX_TRACKED: usize = 8192;

pub fn child_scope(child_session_id: &str) -> String {
    format!("child:{child_session_id}")
}

pub fn format_lifetime_id(agent_id: &str) -> String {
    format!("{agent_id}#{}", uuid::Uuid::new_v4())
}

/// The agent id inside a lifetime id.
pub fn parse_lifetime_id(id: &str) -> Option<&str> {
    let (agent, uuid) = id.rsplit_once('#')?;
    (!agent.is_empty() && uuid.len() == 36 && uuid::Uuid::parse_str(uuid).is_ok()).then_some(agent)
}

#[derive(Clone, Debug)]
struct Lifetime {
    id: String,
    agent_id: String,
    scope: String,
    generation: u64,
    start_seen: bool,
    terminal: bool,
    cancelled: bool,
    request_settled: bool,
    closed_for_start: bool,
    closed_for_request: bool,
    requests: u64,
    order: u64,
}

type Key = (String, String);

pub struct ToolCallLifetimes {
    lifetimes: HashMap<Key, Lifetime>,
    clock: u64,
    keys_by_id: HashMap<String, Key>,
    generations: HashMap<String, u64>,
    create_id: Box<dyn Fn(&str) -> String + Send>,
}

impl Default for ToolCallLifetimes {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolCallLifetimes {
    pub fn new() -> Self {
        Self::with_ids(Box::new(format_lifetime_id))
    }
    pub fn with_ids(create_id: Box<dyn Fn(&str) -> String + Send>) -> Self {
        Self {
            lifetimes: HashMap::new(),
            clock: 0,
            keys_by_id: HashMap::new(),
            generations: HashMap::new(),
            create_id,
        }
    }
    fn generation_of(&self, scope: &str) -> u64 {
        self.generations.get(scope).copied().unwrap_or(0)
    }
    pub fn resolve(&mut self, agent_id: &str, kind: ToolCallEventKind, scope: &str) -> String {
        let mut key = (scope.to_owned(), agent_id.to_owned());
        if !self.lifetimes.contains_key(&key) && kind == ToolCallEventKind::Request {
            if let Some(other) = self.joinable_elsewhere(agent_id, scope) {
                key = (other, agent_id.to_owned());
            }
        }
        let generation = self.generation_of(&key.0);
        let Some(current) = self.lifetimes.get_mut(&key) else {
            return self.open(agent_id, kind, scope);
        };
        match kind {
            ToolCallEventKind::Update => current.id.clone(),
            ToolCallEventKind::Terminal => {
                let id = current.id.clone();
                self.mark_terminal(&key);
                id
            }
            ToolCallEventKind::Request if current.closed_for_request => {
                self.open(agent_id, kind, scope)
            }
            ToolCallEventKind::Request => current.id.clone(),
            ToolCallEventKind::Start if !current.closed_for_start => {
                current.start_seen = true;
                current.id.clone()
            }
            ToolCallEventKind::Start
                if !current.start_seen
                    && !current.cancelled
                    && current.generation == generation =>
            {
                current.start_seen = true;
                current.id.clone()
            }
            ToolCallEventKind::Start => self.open(agent_id, kind, scope),
        }
    }
    /// The first request uses the lifetime id; later ones get their own id.
    pub fn allocate_request_id(&mut self, lifetime_id: &str) -> String {
        let index = match self.by_id(lifetime_id) {
            Some(key) => {
                let l = self.lifetimes.get_mut(&key).unwrap();
                l.requests += 1;
                l.requests - 1
            }
            None => 1,
        };
        if index == 0 {
            lifetime_id.to_owned()
        } else {
            format!("{lifetime_id}~{}", uuid::Uuid::new_v4())
        }
    }
    pub fn begin_generation(&mut self, scope: Option<&str>) {
        let scope = scope.unwrap_or(MAIN_SCOPE);
        let next = self.generation_of(scope) + 1;
        self.generations.insert(scope.to_owned(), next);
        for l in self.lifetimes.values_mut() {
            if l.scope == scope && l.request_settled {
                l.closed_for_start = true;
            }
        }
        self.settle_scope(scope);
    }
    pub fn settle_request(&mut self, lifetime_id: &str) {
        if let Some(key) = self.by_id(lifetime_id) {
            self.lifetimes.get_mut(&key).unwrap().request_settled = true;
        }
    }
    pub fn cancel(&mut self, scope: Option<&str>) {
        let scope = scope.unwrap_or(MAIN_SCOPE);
        for l in self.lifetimes.values_mut() {
            if l.scope == scope && !l.terminal {
                l.cancelled = true;
                l.closed_for_start = true;
            }
        }
    }
    /// The cancelled main-scope prompt has ended.
    pub fn settle_cancelled(&mut self) {
        self.settle_scope(MAIN_SCOPE);
    }
    fn settle_scope(&mut self, scope: &str) {
        let keys: Vec<Key> = self
            .lifetimes
            .iter()
            .filter(|(_, l)| l.scope == scope && l.cancelled && !l.closed_for_request)
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys {
            let l = self.lifetimes.get_mut(&key).unwrap();
            l.closed_for_start = true;
            l.closed_for_request = true;
            self.touch(&key);
        }
    }
    fn joinable_elsewhere(&self, agent_id: &str, scope: &str) -> Option<String> {
        let mut found = None;
        for l in self.lifetimes.values() {
            if l.agent_id != agent_id || l.scope == scope || l.closed_for_request {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some(l.scope.clone());
        }
        found
    }
    fn open(&mut self, agent_id: &str, kind: ToolCallEventKind, scope: &str) -> String {
        let lifetime = Lifetime {
            id: (self.create_id)(agent_id),
            agent_id: agent_id.to_owned(),
            scope: scope.to_owned(),
            generation: self.generation_of(scope),
            start_seen: kind == ToolCallEventKind::Start,
            terminal: false,
            cancelled: false,
            request_settled: false,
            closed_for_start: false,
            closed_for_request: false,
            requests: 0,
            order: 0,
        };
        let id = lifetime.id.clone();
        let key = (scope.to_owned(), agent_id.to_owned());
        if let Some(previous) = self.lifetimes.insert(key.clone(), lifetime) {
            self.keys_by_id.remove(&previous.id);
        }
        self.keys_by_id.insert(id.clone(), key.clone());
        self.touch(&key);
        self.prune();
        if kind == ToolCallEventKind::Terminal && self.lifetimes.contains_key(&key) {
            self.mark_terminal(&key);
        }
        id
    }
    fn touch(&mut self, key: &Key) {
        self.clock += 1;
        if let Some(l) = self.lifetimes.get_mut(key) {
            l.order = self.clock;
        }
    }
    fn mark_terminal(&mut self, key: &Key) {
        let Some(l) = self.lifetimes.get_mut(key) else {
            return;
        };
        if l.terminal {
            return;
        }
        l.terminal = true;
        l.closed_for_start = true;
        l.closed_for_request = true;
        self.touch(key);
        self.prune();
    }
    fn by_id(&self, id: &str) -> Option<Key> {
        let key = self.keys_by_id.get(id)?;
        (self.lifetimes.get(key)?.id == id).then(|| key.clone())
    }
    fn prune(&mut self) {
        if self.lifetimes.len() <= MAX_TRACKED {
            return;
        }
        for tier in 0..3 {
            let mut victims: Vec<(u64, Key)> = self
                .lifetimes
                .iter()
                .filter(|(_, l)| match tier {
                    0 => l.terminal || l.closed_for_request,
                    1 => l.generation < self.generation_of(&l.scope),
                    _ => true,
                })
                .map(|(k, l)| (l.order, k.clone()))
                .collect();
            victims.sort();
            for (_, key) in victims {
                if self.lifetimes.len() <= MAX_TRACKED {
                    return;
                }
                if let Some(l) = self.lifetimes.remove(&key) {
                    self.keys_by_id.remove(&l.id);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ToolCallEventKind::*;

    fn lifetimes() -> ToolCallLifetimes {
        let n = std::sync::atomic::AtomicUsize::new(0);
        ToolCallLifetimes::with_ids(Box::new(move |id| {
            let i = n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            format!("{id}#{i}")
        }))
    }

    #[test]
    fn reused_ids_open_new_lifetimes_after_terminal() {
        let mut l = lifetimes();
        let a = l.resolve("shell:0", Start, MAIN_SCOPE);
        assert_eq!(l.resolve("shell:0", Update, MAIN_SCOPE), a);
        assert_eq!(l.resolve("shell:0", Terminal, MAIN_SCOPE), a);
        assert_eq!(l.resolve("shell:0", Terminal, MAIN_SCOPE), a);
        let b = l.resolve("shell:0", Start, MAIN_SCOPE);
        assert_ne!(a, b);
    }

    #[test]
    fn result_before_call_joins_in_same_generation() {
        let mut l = lifetimes();
        let a = l.resolve("x", Terminal, MAIN_SCOPE);
        assert_eq!(l.resolve("x", Start, MAIN_SCOPE), a);
        l.begin_generation(None);
        assert_ne!(l.resolve("x", Start, MAIN_SCOPE), a);
    }

    #[test]
    fn cancel_closes_start_then_requests_after_settle() {
        let mut l = lifetimes();
        let a = l.resolve("q:0", Start, MAIN_SCOPE);
        l.cancel(None);
        assert_eq!(l.resolve("q:0", Request, MAIN_SCOPE), a);
        assert_eq!(l.resolve("q:0", Update, MAIN_SCOPE), a);
        l.settle_cancelled();
        let b = l.resolve("q:0", Request, MAIN_SCOPE);
        assert_ne!(a, b);
    }

    #[test]
    fn answered_request_closes_start_in_next_generation() {
        let mut l = lifetimes();
        let a = l.resolve("q", Start, MAIN_SCOPE);
        assert_eq!(l.allocate_request_id(&a), a);
        assert!(l.allocate_request_id(&a).starts_with(&format!("{a}~")));
        l.settle_request(&a);
        assert_eq!(l.resolve("q", Start, MAIN_SCOPE), a);
        l.begin_generation(None);
        assert_ne!(l.resolve("q", Start, MAIN_SCOPE), a);
    }

    #[test]
    fn child_request_joins_the_only_open_scope() {
        let mut l = lifetimes();
        let child = child_scope("c1");
        let a = l.resolve("t", Start, &child);
        assert_eq!(l.resolve("t", Request, MAIN_SCOPE), a);
    }

    #[test]
    fn lifetime_ids_parse() {
        let id = format_lifetime_id("shell:0");
        assert_eq!(parse_lifetime_id(&id), Some("shell:0"));
        assert_eq!(parse_lifetime_id("shell:0"), None);
    }
}
