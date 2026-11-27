def skill_fixtures(self, model):
    fixture_root = self.args.skills
    manifest = fixture_root / "manifest.json"
    if not manifest.is_file():
        raise Stop("skills_418_manifest_missing")
    selected = {row["case"]: row for row in json.loads(manifest.read_text())}
    oracles = {"description": "AMBER-418-OK", "model": "COBALT-418-OK",
               "tools": "IVORY-418-READ", "arguments": "raw=[alpha beta]"}
    for case, oracle in oracles.items():
        if case == "model":
            self.cases.append({"model":model,"provider":"openrouter","suite":"skills","case":"model","status":"blocked","reason":"isolated_home_missing_codex_credential; no login authorized"})
            continue
        source = fixture_root / case
        workspace = self.workspace("skill-" + case)
        shutil.copytree(source / ".ego", workspace / ".ego", dirs_exist_ok=True)
        if (source / "sample.txt").is_file():
            shutil.copyfile(source / "sample.txt", workspace / "sample.txt")
        for path in sorted((workspace / ".ego/skills").rglob("*")):
            if path.is_file():
                self.fixture_hashes[f"418/{case}/{path.relative_to(workspace)}"] = digest(path)
        run = self.invoke(model, workspace, selected[case]["prompt"])
        rows = run["rows"]
        invoked = any(row.get("kind") == "SkillInstructionAdmitted" for row in rows)
        tool_names = [row["name"] for row in rows if row.get("kind") == "ToolCallStarted"]
        passed = invoked and oracle in run["stdout"]
        if case == "tools":
            passed &= "read" in tool_names and set(tool_names) <= {"read", "skill"}
        if case == "arguments":
            passed &= all(text in run["stdout"] for text in ("zero=[alpha]", "one=[beta]", "first=[alpha]", "missing=[$2]"))
        override_attributed = None
        if case == "model":
            override_attributed = skill_model_executed(rows, "openai-codex/gpt-6-luna")
            passed &= override_attributed
        self.record(model, "skills", case, [run], passed,
                    {"invoked": invoked, "tool_names": tool_names,
                     "model_override_attributed": override_attributed})
