def invoke(self, model, workspace, prompt=None, session=None, command=None, proxy=None, proxy_evidence=None):
    wire_haiku = model == WIRE_MODEL
    provider = "openrouter" if wire_haiku else model.split("/", 1)[0]
    if provider == "anthropic":
        raise Stop("external blocker: direct Anthropic key capped until 2026-11-01; no retry")
    allowance = self.allowance(provider)
    config = workspace / ".ego/config.toml"
    config.parent.mkdir(exist_ok=True)
    config.write_text(f'model = "{model}"\nthinking = "low"\nbudget = "{allowance}$"\n')
    env = child_env(provider, self.args.ego_env, self.args.router_env)
    if wire_haiku:
        env["ANTHROPIC_API_KEY"] = env.pop("OPENROUTER_API_KEY")
        env["ANTHROPIC_BASE_URL"] = "https://openrouter.ai/api"
        env["EGO_HOME"] = str(WIRE_HOME)
    if provider == "openrouter" and not wire_haiku and getattr(self.args, "operator_home", None):
        env["EGO_HOME"] = str(self.args.operator_home)
    if provider == "openai-codex" and getattr(self.args, "diagnostic_dir", None):
        self.args.diagnostic_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
        env["EGO_LOG"] = "off,ego_ai::codex_session=debug,ego_tools::http::websocket=debug"
        env["EGO_LOG_FILE"] = str(self.args.diagnostic_dir / f"{self.run_id}-{self.serial:03}-{uuid.uuid4().hex[:8]}.log")

    if proxy:
        env[{"openai": "OPENAI_BASE_URL", "anthropic": "ANTHROPIC_BASE_URL"}["anthropic" if wire_haiku else provider]] = proxy
    common = ["-C", str(workspace), "--session-dir", str(self.store)]
    if command:
        argv = [str(self.args.binary), *command, *common, "--output", "json"]
        if command[0] == "compact":
            argv += ["--budget", f"{allowance}$", "--max-output-tokens", "2048"]
    else:
        argv = [str(self.args.binary), "run", *common, "-m", model, "--thinking", "low",
                "--instructions", str(workspace / "AGENTS.md"), "--trust-workspace",
                "--mode", "plan", "--sandbox", "workspace", "--tools", "read,skill" if (workspace / ".ego/skills").is_dir() else "read",
                "--budget", f"{allowance}$", "--max-output-tokens", "1024", "--max-turns", "6", "--timeout", "120s" if model == "openrouter/openai/gpt-6-luna" else "50s",
                "--retry", "1", "--output", "text", "--session-id", session or str(uuid.uuid4()), prompt]
    before = events(self.store)
    timed_out = False
    try:
        completed = subprocess.run(argv, capture_output=True, text=True, env=env,
                                   cwd=workspace, timeout=135 if model == "openrouter/openai/gpt-6-luna" else 65)
        code, stdout, stderr = completed.returncode, completed.stdout, completed.stderr
    except subprocess.TimeoutExpired as error:
        timed_out = True
        stderr = error.stderr or ""
        if isinstance(stderr, bytes):
            stderr = stderr.decode("utf-8", errors="replace")
        code, stdout = -1, ""
    # Private diagnostics are not part of the append-only public results.
    # Redact the keys already supplied to this child; never inspect a store.
    diagnostic = stderr
    for key in KEYS:
        if env.get(key):
            diagnostic = diagnostic.replace(env[key], "[redacted]")
    stderr_path = workspace / f"stderr-{uuid.uuid4().hex}.txt"
    descriptor = os.open(stderr_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w") as output:
        output.write(diagnostic)
    after = events(self.store)
    rows = [value for key, value in after.items() if key not in before]
    proxy_zero_booked = False
    if (proxy_evidence is not None and proxy_evidence.fault == "before"
            and proxy_evidence.injected):
        # Only the first incoming request is dropped without an upstream call.
        # Keep ego's original journal untouched; annotate the runner projection.
        first_attempt = next((r["attempt"]["attempt"] for r in rows
                              if r.get("kind") == "GenerationStarted"), None)
        projected = []
        for row in rows:
            if (row.get("kind") == "UsageUpdated"
                    and row.get("attempt", {}).get("attempt") == first_attempt
                    and (row.get("cost_delta") or {}).get("cost") == "unknown"):
                row = dict(row, cost_delta={"cost": "known",
                    "amount": {"amount": "0", "currency": "USD"},
                    "attribution": {"source": "runner_proxy_not_forwarded",
                                    "revision": "live-conformance/v1"}})
                proxy_zero_booked = True
            projected.append(row)
        rows = projected
    metrics = usage_metrics(rows)
    metrics["proxy_proven_zero_attempt"] = proxy_zero_booked
    cost = Decimal(metrics["cost_usd"])
    self.spent += cost
    self.by_provider[provider] = self.by_provider.get(provider, Decimal(0)) + cost
    # An interrupted attempt without usage has unknown spend. A pre-delivery
    # refusal has no GenerationStarted, and is safe to continue measuring.
    starts = {r["attempt"]["attempt"] for r in rows if r.get("kind") == "GenerationStarted"}
    metered = {r["attempt"]["attempt"] for r in rows if r.get("kind") == "UsageUpdated"}
    unsettled_cost = starts - metered
    settled_not_sent = {r["attempt"]["attempt"] for r in rows
                        if r.get("kind") == "AttemptSettled"
                        and r.get("outcome", {}).get("delivery") == "definitely_unsent"}
    self.uncertain |= metrics["unknown_cost"] or bool(unsettled_cost - settled_not_sent)
    reason = "timeout" if timed_out else ("reasoning_capability_unknown" if "nobody stated whether the model accepts" in stderr else ("budget_refused" if "budget denied" in stderr or "NoPricingQuote" in stderr else
                                         ("process_failed" if code else "completed")))
    return {"exit": code, "reason": reason, "metrics": metrics, "rows": rows,
            "stdout": stdout, "stderr_path": str(stderr_path),
            "new_sessions": sorted({key[0] for key in after if key not in before})}
