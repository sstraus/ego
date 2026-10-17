_ego() {
    local i cur prev opts cmd
    COMPREPLY=()
    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
        cur="$2"
    else
        cur="${COMP_WORDS[COMP_CWORD]}"
    fi
    prev="$3"
    cmd=""
    opts=""

    for i in "${COMP_WORDS[@]:0:COMP_CWORD}"
    do
        case "${cmd},${i}" in
            ",$1")
                cmd="ego"
                ;;
            ego,a2a)
                cmd="ego__subcmd__a2a"
                ;;
            ego,acp)
                cmd="ego__subcmd__acp"
                ;;
            ego,auth)
                cmd="ego__subcmd__auth"
                ;;
            ego,compact)
                cmd="ego__subcmd__compact"
                ;;
            ego,config)
                cmd="ego__subcmd__config"
                ;;
            ego,continue)
                cmd="ego__subcmd__continue"
                ;;
            ego,doctor)
                cmd="ego__subcmd__doctor"
                ;;
            ego,export)
                cmd="ego__subcmd__export"
                ;;
            ego,fork)
                cmd="ego__subcmd__fork"
                ;;
            ego,hooks)
                cmd="ego__subcmd__hooks"
                ;;
            ego,ls)
                cmd="ego__subcmd__ls"
                ;;
            ego,mcp)
                cmd="ego__subcmd__mcp"
                ;;
            ego,mcp-server)
                cmd="ego__subcmd__mcp__subcmd__server"
                ;;
            ego,models)
                cmd="ego__subcmd__models"
                ;;
            ego,orchestrator)
                cmd="ego__subcmd__orchestrator"
                ;;
            ego,resume)
                cmd="ego__subcmd__resume"
                ;;
            ego,rm)
                cmd="ego__subcmd__rm"
                ;;
            ego,roots)
                cmd="ego__subcmd__roots"
                ;;
            ego,rules)
                cmd="ego__subcmd__rules"
                ;;
            ego,run)
                cmd="ego__subcmd__run"
                ;;
            ego,setup)
                cmd="ego__subcmd__setup"
                ;;
            ego,usage)
                cmd="ego__subcmd__usage"
                ;;
            ego,worktree)
                cmd="ego__subcmd__worktree"
                ;;
            ego__subcmd__auth,login)
                cmd="ego__subcmd__auth__subcmd__login"
                ;;
            ego__subcmd__auth,logout)
                cmd="ego__subcmd__auth__subcmd__logout"
                ;;
            ego__subcmd__auth,status)
                cmd="ego__subcmd__auth__subcmd__status"
                ;;
            ego__subcmd__config,batch)
                cmd="ego__subcmd__config__subcmd__batch"
                ;;
            ego__subcmd__config,capabilities)
                cmd="ego__subcmd__config__subcmd__capabilities"
                ;;
            ego__subcmd__config,edit)
                cmd="ego__subcmd__config__subcmd__edit"
                ;;
            ego__subcmd__config,get)
                cmd="ego__subcmd__config__subcmd__get"
                ;;
            ego__subcmd__config,ls)
                cmd="ego__subcmd__config__subcmd__ls"
                ;;
            ego__subcmd__config,set)
                cmd="ego__subcmd__config__subcmd__set"
                ;;
            ego__subcmd__config,snapshot)
                cmd="ego__subcmd__config__subcmd__snapshot"
                ;;
            ego__subcmd__config,unset)
                cmd="ego__subcmd__config__subcmd__unset"
                ;;
            ego__subcmd__hooks,ls)
                cmd="ego__subcmd__hooks__subcmd__ls"
                ;;
            ego__subcmd__mcp,add)
                cmd="ego__subcmd__mcp__subcmd__add"
                ;;
            ego__subcmd__mcp,login)
                cmd="ego__subcmd__mcp__subcmd__login"
                ;;
            ego__subcmd__mcp,ls)
                cmd="ego__subcmd__mcp__subcmd__ls"
                ;;
            ego__subcmd__mcp,reload)
                cmd="ego__subcmd__mcp__subcmd__reload"
                ;;
            ego__subcmd__mcp,rm)
                cmd="ego__subcmd__mcp__subcmd__rm"
                ;;
            ego__subcmd__orchestrator,brief)
                cmd="ego__subcmd__orchestrator__subcmd__brief"
                ;;
            ego__subcmd__roots,add)
                cmd="ego__subcmd__roots__subcmd__add"
                ;;
            ego__subcmd__roots,ls)
                cmd="ego__subcmd__roots__subcmd__ls"
                ;;
            ego__subcmd__roots,rm)
                cmd="ego__subcmd__roots__subcmd__rm"
                ;;
            ego__subcmd__rules,allow)
                cmd="ego__subcmd__rules__subcmd__allow"
                ;;
            ego__subcmd__rules,deny)
                cmd="ego__subcmd__rules__subcmd__deny"
                ;;
            ego__subcmd__rules,explain)
                cmd="ego__subcmd__rules__subcmd__explain"
                ;;
            ego__subcmd__rules,ls)
                cmd="ego__subcmd__rules__subcmd__ls"
                ;;
            ego__subcmd__rules,remember)
                cmd="ego__subcmd__rules__subcmd__remember"
                ;;
            ego__subcmd__rules,revoke)
                cmd="ego__subcmd__rules__subcmd__revoke"
                ;;
            ego__subcmd__worktree,cleanup)
                cmd="ego__subcmd__worktree__subcmd__cleanup"
                ;;
            ego__subcmd__worktree,show)
                cmd="ego__subcmd__worktree__subcmd__show"
                ;;
            *)
                ;;
        esac
    done

    case "${cmd}" in
        ego)
            opts="-h --help run resume continue fork rm compact export ls usage models hooks doctor setup acp a2a auth mcp config roots rules mcp-server orchestrator worktree"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 1 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__a2a)
            opts="-C -h --session-dir --sandbox --listen --allow-public-bind --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --session-dir)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -C)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --sandbox)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --listen)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__acp)
            opts="-C -h --session-dir --sandbox --read-only-dir --system --bare --append-system --instructions --tools --no-tools --allow --deny --profile --listen --allow-public-bind --max-connections --connection-lifetime --max-ingress-bytes --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --session-dir)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -C)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --sandbox)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --read-only-dir)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --system)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --append-system)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --instructions)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --tools)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --allow)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --deny)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --profile)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --listen)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-connections)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --connection-lifetime)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-ingress-bytes)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__auth)
            opts="-h --help login logout status"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__auth__subcmd__login)
            opts="-h --device --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__auth__subcmd__logout)
            opts="-h --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__auth__subcmd__status)
            opts="-h --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__compact)
            opts="-C -h --budget --max-output-tokens --request-id --session-dir --output --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --budget)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-output-tokens)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --request-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --session-dir)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -C)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --output)
                    COMPREPLY=($(compgen -W "text json jsonl" -- "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__config)
            opts="-h --help capabilities snapshot batch get set unset ls edit"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__config__subcmd__batch)
            opts="-h --profile --set --if-revision --unset --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --profile)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --set)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --if-revision)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --unset)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__config__subcmd__capabilities)
            opts="-h --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__config__subcmd__edit)
            opts="-h --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__config__subcmd__get)
            opts="-h --profile --json --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --profile)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__config__subcmd__ls)
            opts="-h --profile --json --effective --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --profile)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__config__subcmd__set)
            opts="-h --profile --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --profile)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__config__subcmd__snapshot)
            opts="-h --profile --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --profile)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__config__subcmd__unset)
            opts="-h --profile --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --profile)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__continue)
            opts="-f -m -C -c -q -h --answer --worktree --file --var --attach --model --offline --sandbox --thinking --read-only-dir --system --bare --append-system --instructions --tools --no-tools --allow --deny --profile --trust-workspace --session --session-id --ephemeral --session-dir --output --artifact-dir --schema --schema-retries --retry --fallback --budget --max-output-tokens --goal --no-context-tool --context-clear --context-interval --goal-turns --goal-check-command --max-turns --timeout --mcp --mode --plan --yolo --set --quiet --no-color --plain --vim --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --answer)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --worktree)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --file)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -f)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --var)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --attach)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --model)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -m)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --sandbox)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --thinking)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --read-only-dir)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --system)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --append-system)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --instructions)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --tools)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --allow)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --deny)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --profile)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --session)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --session-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --session-dir)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -C)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --output)
                    COMPREPLY=($(compgen -W "text json jsonl" -- "${cur}"))
                    return 0
                    ;;
                --artifact-dir)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --schema)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --schema-retries)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --retry)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --fallback)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --budget)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-output-tokens)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --goal)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --context-interval)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --goal-turns)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --goal-check-command)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-turns)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --timeout)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --mcp)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --mode)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --set)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__doctor)
            opts="-h --json --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__export)
            opts="-C -h --sanitize --format --follow --after --file --session-dir --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --format)
                    COMPREPLY=($(compgen -W "md json jsonl transcript" -- "${cur}"))
                    return 0
                    ;;
                --after)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --file)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --session-dir)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -C)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__fork)
            opts="-C -h --at --session-dir --output --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --at)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --session-dir)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -C)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --output)
                    COMPREPLY=($(compgen -W "text json jsonl" -- "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__hooks)
            opts="-h --help ls"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__hooks__subcmd__ls)
            opts="-h --json --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__ls)
            opts="-C -h --all --session-dir --output --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --session-dir)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -C)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --output)
                    COMPREPLY=($(compgen -W "text json jsonl" -- "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__mcp)
            opts="-h --help add rm ls login reload"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__mcp__subcmd__server)
            opts="-C -h --session-dir --max-concurrent-runs --retained-requests --max-resource-bytes --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --session-dir)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -C)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-concurrent-runs)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --retained-requests)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-resource-bytes)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__mcp__subcmd__add)
            opts="-h --stdio --http --cwd --inherit-env --profile --auth --allow-insecure-loopback --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --http)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --cwd)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --inherit-env)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --profile)
                    COMPREPLY=($(compgen -W "current legacy-sse" -- "${cur}"))
                    return 0
                    ;;
                --auth)
                    COMPREPLY=($(compgen -W "none oauth" -- "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__mcp__subcmd__login)
            opts="-h --client-id --cimd --dcr --callback-port --refresh --issuer --account --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --client-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --cimd)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --callback-port)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --issuer)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --account)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__mcp__subcmd__ls)
            opts="-h --json --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__mcp__subcmd__reload)
            opts="-h --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__mcp__subcmd__rm)
            opts="-h --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__models)
            opts="-h --provider --refresh --json --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --provider)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__orchestrator)
            opts="-h --help brief"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__orchestrator__subcmd__brief)
            opts="-h --root --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --root)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__resume)
            opts="-f -m -C -c -q -h --answer --worktree --file --var --attach --model --offline --sandbox --thinking --read-only-dir --system --bare --append-system --instructions --tools --no-tools --allow --deny --profile --trust-workspace --session --session-id --ephemeral --session-dir --output --artifact-dir --schema --schema-retries --retry --fallback --budget --max-output-tokens --goal --no-context-tool --context-clear --context-interval --goal-turns --goal-check-command --max-turns --timeout --mcp --mode --plan --yolo --set --quiet --no-color --plain --vim --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --answer)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --worktree)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --file)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -f)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --var)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --attach)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --model)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -m)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --sandbox)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --thinking)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --read-only-dir)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --system)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --append-system)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --instructions)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --tools)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --allow)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --deny)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --profile)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --session)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --session-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --session-dir)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -C)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --output)
                    COMPREPLY=($(compgen -W "text json jsonl" -- "${cur}"))
                    return 0
                    ;;
                --artifact-dir)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --schema)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --schema-retries)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --retry)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --fallback)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --budget)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-output-tokens)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --goal)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --context-interval)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --goal-turns)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --goal-check-command)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-turns)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --timeout)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --mcp)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --mode)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --set)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__rm)
            opts="-C -h --yes --force --session-dir --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --session-dir)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -C)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__roots)
            opts="-h --help add rm ls"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__roots__subcmd__add)
            opts="-h --profile --access --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --profile)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --access)
                    COMPREPLY=($(compgen -W "read read-write" -- "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__roots__subcmd__ls)
            opts="-h --profile --json --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --profile)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__roots__subcmd__rm)
            opts="-h --profile --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --profile)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__rules)
            opts="-h --help ls allow deny revoke remember explain"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__rules__subcmd__allow)
            opts="-h --profile --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --profile)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__rules__subcmd__deny)
            opts="-h --profile --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --profile)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__rules__subcmd__explain)
            opts="-C -h --profile --json --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --profile)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -C)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__rules__subcmd__ls)
            opts="-h --profile --json --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --profile)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__rules__subcmd__remember)
            opts="-C -h --profile --yes --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --profile)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -C)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__rules__subcmd__revoke)
            opts="-h --profile --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --profile)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__run)
            opts="-f -m -C -c -q -h --worktree --file --var --attach --model --offline --sandbox --thinking --read-only-dir --system --bare --append-system --instructions --tools --no-tools --allow --deny --profile --trust-workspace --session --session-id --ephemeral --session-dir --output --artifact-dir --schema --schema-retries --retry --fallback --budget --max-output-tokens --goal --no-context-tool --context-clear --context-interval --goal-turns --goal-check-command --max-turns --timeout --mcp --mode --plan --yolo --set --quiet --no-color --plain --vim --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --worktree)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --file)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -f)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --var)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --attach)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --model)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -m)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --sandbox)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --thinking)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --read-only-dir)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --system)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --append-system)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --instructions)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --tools)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --allow)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --deny)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --profile)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --session)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --session-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --session-dir)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -C)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --output)
                    COMPREPLY=($(compgen -W "text json jsonl" -- "${cur}"))
                    return 0
                    ;;
                --artifact-dir)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --schema)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --schema-retries)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --retry)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --fallback)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --budget)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-output-tokens)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --goal)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --context-interval)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --goal-turns)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --goal-check-command)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-turns)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --timeout)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --mcp)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --mode)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --set)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__setup)
            opts="-h --items --judge --profile --directory --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --items)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --judge)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --profile)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --directory)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__usage)
            opts="-C -h --period --json --all-roles --session-dir --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --period)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --session-dir)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -C)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__worktree)
            opts="-h --help show cleanup"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__worktree__subcmd__cleanup)
            opts="-h --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        ego__subcmd__worktree__subcmd__show)
            opts="-h --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
    esac
}

if [[ "${BASH_VERSINFO[0]}" -eq 4 && "${BASH_VERSINFO[1]}" -ge 4 || "${BASH_VERSINFO[0]}" -gt 4 ]]; then
    complete -F _ego -o nosort -o bashdefault -o default ego
else
    complete -F _ego -o bashdefault -o default ego
fi
