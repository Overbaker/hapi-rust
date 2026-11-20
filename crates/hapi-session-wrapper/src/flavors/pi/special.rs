use super::*;
pub(super) const BUILTINS: [(&str, &str); 4] = [
    ("help", "Show supported HAPI Pi slash commands"),
    (
        "compact",
        "Compress conversation history to save context (optional custom instructions)",
    ),
    (
        "session",
        "Show Pi session stats (tokens, cost, context usage)",
    ),
    ("model", "Show or switch the Pi model, e.g. /model gpt-5.2"),
];
impl Runtime {
    pub(super) async fn special(&mut self, text: &str) -> Result<bool, String> {
        let Some(command) = text.trim().strip_prefix('/') else {
            return Ok(false);
        };
        let (name, args) = command
            .split_once(char::is_whitespace)
            .unwrap_or((command, ""));
        let name = name.to_lowercase();
        let args = args.trim();
        if self.commands.as_array().is_some_and(|list| {
            list.iter().any(|cmd| {
                cmd["name"]
                    .as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case(&name))
            })
        }) {
            return Ok(false);
        }
        match name.as_str(){
            "help"=>self.notice("Supported HAPI Pi commands:\n/compact [instructions] — compress conversation history to save context\n/session — show session stats (tokens, cost, context usage)\n/model [modelId] — show or switch the active model\n/help — show this message"),
            "session"=>match self.transport.rpc(json!({"type":"get_session_stats"})).await{
                Ok(stats)=>{
                    let mut lines=vec!["📊 Pi session stats".into()];
                    if let Some(n)=stats["totalMessages"].as_u64(){lines.push(format!("Messages: {n}"))}
                    if stats["tokens"]["total"].is_number(){
                        let tokens=&stats["tokens"];let fields=[("total","total"),("input","in"),("output","out"),("cacheRead","cacheR"),("cacheWrite","cacheW")];
                        let values=fields.iter().filter_map(|(k,label)|tokens[*k].as_u64().map(|n|format!("{label} {n}"))).collect::<Vec<_>>();
                        lines.push(format!("Tokens: {}",values.join(" · ")))
                    }
                    if let Some(n)=stats["cost"].as_f64(){lines.push(format!("Cost: ${n:.4}"))}
                    let context=&stats["contextUsage"];if let Some(n)=context["tokens"].as_u64(){let limit=context["contextWindow"].as_u64().map(|n|format!(" / {n}")).unwrap_or_default();let percent=context["percent"].as_f64().map(|n|format!(" ({n}%)")).unwrap_or_default();lines.push(format!("Context: {n}{limit} tokens{percent}"))}
                    self.notice(&lines.join("\n"))
                }
                Err(e)=>self.notice(&format!("⚠️ Could not read Pi session stats: {e}"))
            },
            "model"=>{
                let qualified=|m:&Value|format!("{}/{}",m["provider"].as_str().unwrap_or("unknown"),m["modelId"].as_str().unwrap_or(""));
                if args.is_empty(){
                    let mut current=self.runtime["model"].as_str().unwrap_or("unset").to_owned();
                    if let Some(provider)=self.provider.as_str(){current=format!("{provider}/{current}")}
                    let catalog=self.models.iter().map(qualified).collect::<Vec<_>>().join(", ");
                    self.notice(&format!("Current model: {current}.\nAvailable: {}",if catalog.is_empty(){"unknown — use /model <modelId>"}else{&catalog}));
                }else{
                    let exact=self.models.iter().find(|m|qualified(m)==args);
                    let bare:Vec<_>=self.models.iter().filter(|m|m["modelId"]==args).collect();
                    if exact.is_none() && bare.len()>1{self.notice(&format!("⚠️ Ambiguous model: {args}. Use {}.",bare.into_iter().map(qualified).collect::<Vec<_>>().join(", ")))}
                    else if let Some(model)=exact.or_else(||bare.first().copied()){let config=json!({"model":model});self.config(&config).await?;self.notice(&format!("Model switched to {args}"))}
                    else{self.notice(&format!("⚠️ Unknown model: {args}. Use /model to list available models."))}
                }
            }
            "compact"=>{
                self.dialogs.expire(true,&self.transport,&self.client).await?;
                let transport=self.transport.clone();
                let mut cmd=json!({"type":"compact"});if !args.is_empty(){cmd["customInstructions"]=json!(args)}
                self.compact=Some(tokio::spawn(async move{transport.rpc(cmd).await}));
                self.streaming=true;self.alive();
            }
            "login"|"logout"|"llama"|"scoped-models"|"settings"|"resume"|"new"|"name"|"tree"|"trust"|"fork"|"clone"|"copy"|"export"|"import"|"share"|"reload"|"hotkeys"|"changelog"|"quit"=>self.notice(&format!("⚠️ /{name} is a Pi terminal-only command and cannot run from HAPI web. Supported here: /compact, /session, /model, /help.")),
            _=>return Ok(false)
        }
        Ok(true)
    }

    pub(super) fn usage(&self, stats: &Value, usage: &Value, estimate: Option<u64>) {
        let context = &stats["contextUsage"];
        let total = usage["totalTokens"].as_u64().filter(|n| *n > 0);
        if estimate.is_none() && (total.is_none() || context.get("tokens") == Some(&Value::Null)) {
            return;
        }
        use hapi_wrapper_core::{
            acp::types::{AgentMessage, Usage},
            agent::message_converter::convert_agent_message,
        };
        let usage = AgentMessage::Usage(Usage {
            input_tokens: usage["input"].as_f64().unwrap_or(0.0),
            output_tokens: usage["output"].as_f64().unwrap_or(0.0),
            cache_read_tokens: usage["cacheRead"].as_f64(),
            cache_creation_tokens: usage["cacheWrite"].as_f64(),
            total_tokens: usage["totalTokens"].as_f64(),
            context_tokens: estimate
                .or_else(|| context["tokens"].as_u64())
                .or(total)
                .map(|n| n as f64),
            context_window: context["contextWindow"].as_f64(),
            ..Default::default()
        });
        if let Some(row) = convert_agent_message(&usage, self.runtime["model"].as_str()) {
            self.client.send_agent_message(row, None)
        }
    }
}
