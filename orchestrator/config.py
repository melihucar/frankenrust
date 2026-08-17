"""Per-role agent/model table for the orchestrator.

Edit this file for the weekly split change. Everything defaults to opencode
right now: the run is testing end to end on opencode's free models, and
claude/codex stay wired so the split can be restored per role (implementation
on a cheap agent, judgement on claude) the day the quota resets.

Precedence, Laravel-style: values in orchestrator/.env (gitignored; copy
.env.example) win over the defaults here, and real environment variables win
over both. There is no other configuration surface -- every FR_* knob this
file reads is documented in .env.example.
"""
from __future__ import annotations

import os
from pathlib import Path

CONFIG_DIR = Path(__file__).parent

AGENTS = ("claude", "codex", "opencode")

# Which agent runs which role. Implementation is bulk mechanical translation
# against a spec that already names the files, which a cheap opencode model
# handles; critique, review and fixing are where judgement matters (unsound
# unsafe, thread-affinity bugs a green suite misses), so those are the roles
# to move back to claude when the quota allows.
ROLE_AGENT = {
    "implementer": "opencode",
    "fixer": "opencode",
    "critic": "opencode",
    "reviewer": "opencode",
    "planner": "opencode",
    "resolver": "opencode",
    "unblocker": "opencode",
    "retro": "opencode",
}

# claude's model table. Kept wired even while nothing runs on claude, so
# flipping FR_AGENT_<ROLE> back is a one-line change per role.
MODELS = {
    "implementer": "claude-sonnet-5",
    "critic": "claude-opus-5",
    "reviewer": "claude-opus-5",
    "fixer": "claude-opus-5",
    "planner": "claude-opus-5",
    "resolver": "claude-opus-5",
    "unblocker": "claude-opus-5",
}

# opencode's model table -- free models by default, since the run is testing
# end to end and a paid model buys nothing there.
OPENCODE_MODELS = {role: "opencode/deepseek-v4-flash-free" for role in MODELS}

# The `duel` Agent: alternates between two agents on failure. With the
# all-opencode roster the agent pair is a single opencode and the *models*
# alternate instead -- attempt 1 runs the first DUEL_MODELS entry, attempt 2
# the second, and so on.
DUEL_AGENTS = ["opencode"]
DUEL_MODELS = ["opencode/deepseek-v4-flash-free", "opencode/hy3-free"]

# Who was *asked* to review. This is the roster, not the attendance sheet --
# review_stage() seeds its results from it so a reviewer that never came back
# still counts as one that owes a verdict, and retries only the indices that
# went silent. What actually ran is invoke()'s first return value, and that is
# what names a reviewer in public. Reviewer 2 is the cross-model slot the day
# the split returns (claude reviews 1, a cheaper model reviews 2).
REVIEWER_AGENTS = {1: "opencode", 2: "opencode"}

# The strongest model available for the final attempt of a failing issue,
# per agent. opencode's escalation defaults to the same free model, since the
# free fleet has no stronger tier; FR_MODEL_ESCALATE stays wired for the day
# claude comes back as the judgement tier.
ESCALATED_MODEL = "claude-opus-5"
OPENCODE_ESCALATED_MODEL = "opencode/deepseek-v4-flash-free"


def _load_dotenv() -> None:
    """Load the .env file into the environment without clobbering anything.

    Real environment variables win: a value already set on the process is
    left alone, exactly phpdotenv's immutable mode. The path is overridable
    via FR_ENV_FILE so a test can point at a scratch file.
    """
    env = Path(os.environ.get("FR_ENV_FILE", CONFIG_DIR / ".env"))
    if not env.exists():
        return
    for line in env.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, _, value = line.partition("=")
        key, value = key.strip(), value.strip().strip('"').strip("'")
        if key:
            os.environ.setdefault(key, value)


# The env-var suffix per role. Not role.upper(): planner is FR_AGENT_PLAN,
# not FR_AGENT_PLANNER, and those names are what the loop's operators
# already have in their environments.
ROLE_ENV = {
    "implementer": "IMPL",
    "fixer": "FIX",
    "critic": "CRITIC",
    "reviewer": "REVIEW",
    "planner": "PLAN",
    "resolver": "RESOLVE",
    "unblocker": "UNBLOCK",
    "retro": "RETRO",
}


def _apply_env() -> None:
    """Overlay .env / environment overrides onto the tables above."""
    global ESCALATED_MODEL, OPENCODE_ESCALATED_MODEL
    for role in ROLE_AGENT:
        ROLE_AGENT[role] = os.environ.get(f"FR_AGENT_{ROLE_ENV[role]}", ROLE_AGENT[role])
    for role in MODELS:
        MODELS[role] = os.environ.get(f"FR_MODEL_{ROLE_ENV[role]}", MODELS[role])
    for role in OPENCODE_MODELS:
        OPENCODE_MODELS[role] = os.environ.get(
            f"FR_OPENCODE_MODEL_{ROLE_ENV[role]}", OPENCODE_MODELS[role])
    for slot in REVIEWER_AGENTS:
        REVIEWER_AGENTS[slot] = os.environ.get(
            f"FR_REVIEWER{slot}", REVIEWER_AGENTS[slot])
    agents = os.environ.get("FR_DUEL_AGENTS", ",".join(DUEL_AGENTS))
    DUEL_AGENTS[:] = [a.strip() for a in agents.split(",") if a.strip()]
    models = os.environ.get("FR_DUEL_MODELS", ",".join(DUEL_MODELS))
    DUEL_MODELS[:] = [m.strip() for m in models.split(",") if m.strip()]
    ESCALATED_MODEL = os.environ.get("FR_MODEL_ESCALATE", ESCALATED_MODEL)
    OPENCODE_ESCALATED_MODEL = os.environ.get(
        "FR_OPENCODE_MODEL_ESCALATE", OPENCODE_MODELS["implementer"])


def _validate() -> None:
    """Fail at boot, not mid-run: an unknown agent in the tables would only
    surface as a ValueError from resolve() on the first issue that hits the
    role, which a fleet finds hours in."""
    known = set(AGENTS)
    for role, agent in ROLE_AGENT.items():
        if agent not in known:
            raise ValueError(f"config: unknown agent {agent!r} for role {role!r} "
                             f"(known: {', '.join(AGENTS)})")
    for slot, agent in REVIEWER_AGENTS.items():
        if agent not in known:
            raise ValueError(f"config: unknown reviewer {slot}: {agent!r} "
                             f"(known: {', '.join(AGENTS)})")
    for agent in DUEL_AGENTS:
        if agent not in known:
            raise ValueError(f"config: unknown duel agent {agent!r} "
                             f"(known: {', '.join(AGENTS)})")


_load_dotenv()
_apply_env()
_validate()