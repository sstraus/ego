def recovery(self, model):
    provider = model.split("/", 1)[0]
    if provider not in ("openai", "anthropic"):
        self.cases.append({"model": model, "provider": provider, "suite": "recovery",
                           "case": "fault-proxy", "status": "not_applicable",
                           "reason": "provider_has_no_supported_base_url_override"})
        return
    for fault in ("before", "after"):
        workspace = self.workspace("recovery-" + fault)
        proxy = FaultProxy(provider, fault)
        if model == WIRE_MODEL: proxy.origin = "openrouter.ai"
        url = proxy.start()
        if model == WIRE_MODEL: url = url.rstrip("/") + "/api"
        try:
            run = self.invoke(model, workspace,
                              "Write the integers from 1 to 80 separated by spaces. Do not use tools.", proxy=url, proxy_evidence=proxy)
        finally:
            proxy.close()
        passed = (proxy.injected and proxy.text_seen and proxy.upstream_statuses == [200]
                  and not proxy.transport_errors) and ((proxy.requests == 2 and run["exit"] == 0) if fault == "before"
                                     else (proxy.requests == 1 and proxy.text_seen and run["exit"] != 0))
        self.record(model, "recovery", fault, [run], passed,
                    {"proxy_requests": proxy.requests, "fault_injected": proxy.injected,
                     "real_text_seen": proxy.text_seen, "upstream_statuses": proxy.upstream_statuses,
                     "transport_errors": proxy.transport_errors}, baseline=True)
