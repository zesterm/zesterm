#!/usr/bin/env node
/**
 * Apply the sigx-standard `main` protection to a repo — branch rules as code.
 *
 * Branch protection lives in GitHub settings, not in the repo's files, so it
 * silently drifts between repos. This script makes it reproducible: run it once
 * per repo (and re-run any time to reconcile).
 *
 * What it enforces:
 *   Repo merge settings:
 *     - Squash-only merges (merge commits + rebase merges disabled) → linear history.
 *     - Squash commit message = PR title + PR body (instead of GitHub's default
 *       COMMIT_MESSAGES concatenation), so PR descriptions double as the commit
 *       body — write them accordingly. NOTE: GitHub still auto-appends
 *       `Co-authored-by:` trailers to ANY message it generates itself, in every
 *       squash-message mode, whenever a branch-commit author differs from the
 *       merging account. The sigx standard forbids those trailers, so the merge
 *       step (AGENTS.md step 6) must pass --subject/--body explicitly — an
 *       explicit message is used verbatim, with nothing appended.
 *     - Auto-delete head branches after merge.
 *   Ruleset "sigx-standard: protect main" on `main`:
 *     - No direct pushes — changes land via PR only.
 *     - PR required: `--approvals N` approving reviews (default 1; pass 0 for a
 *       solo repo where the owner merges without a separate approval), stale
 *       approvals dismissed on new commits, CODEOWNERS review when approvals >= 1,
 *       review threads must resolve.
 *     - No force-push and no deletion of `main`.
 *     - (Optional) required status checks green before merge — pass --checks.
 *
 * Required status checks are OPT-IN via --checks because a wrong context name
 * would block ALL merges. Discover your real check names on any open PR with
 * `gh pr checks <pr>` (or the PR "Checks" tab), then re-run with them.
 *
 * Usage:
 *   node scripts/apply-branch-protection.mjs <owner/repo> [--checks "a; b; c"] [--approvals N] [--dry-run]
 *   (--checks is semicolon-separated — matrix check names contain commas.)
 *
 * Examples:
 *   node scripts/apply-branch-protection.mjs signalxjs/core
 *   node scripts/apply-branch-protection.mjs signalxjs/core --checks "test (ubuntu-latest, 22); verify-pack"
 *
 * Requirements: `gh` CLI authenticated (`gh auth login`) with admin on the repo.
 */
import { spawnSync } from 'node:child_process';

const RULESET_NAME = 'sigx-standard: protect main';
const DEFAULT_BRANCH = 'main';

// ── args ─────────────────────────────────────────────────────────────────────
const argv = process.argv.slice(2);
let repo;
let checks = [];
let dryRun = false;
let approvals = 1; // required approving reviews; 0 = PR required but owner may self-merge
// Off by default: see the `strict_required_status_checks_policy` comment below
// for why requiring an up-to-date branch costs more than it protects once more
// than one branch is in flight.
let strict = false;
for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--dry-run') dryRun = true;
    else if (a === '--strict') strict = true;
    else if (a === '--checks') {
        const v = argv[++i];
        // Reject a missing value or a following flag (e.g. `--checks --dry-run`)
        // rather than silently consuming it. Only `--`-prefixed tokens are this
        // script's flags; a single-dash check-run name (e.g. "-lint") is allowed.
        if (!v || v.startsWith('--')) die('--checks needs a value, e.g. --checks "test (ubuntu-latest, 22); verify-pack"');
        // Split on ';' — NOT ',' — because matrix check-run names contain commas
        // ("test (ubuntu-latest, 22)"). Repeatable: values accumulate across flags.
        checks.push(...v.split(';').map((s) => s.trim()).filter(Boolean));
    } else if (a === '--approvals') {
        const v = argv[++i];
        if (!/^\d+$/.test(v ?? '')) die('--approvals needs a non-negative integer, e.g. --approvals 0');
        approvals = Number(v);
    } else if (!a.startsWith('-') && !repo) repo = a;
    else die(`Unexpected argument: ${a}`);
}
if (!repo || !/^[^/]+\/[^/]+$/.test(repo)) {
    die('Usage: node scripts/apply-branch-protection.mjs <owner/repo> [--checks "a; b"] [--approvals N] [--strict] [--dry-run]\n' +
        '  --checks  semicolon-separated check-run names (repeatable). Use ";" not "," —\n' +
        '            matrix names contain commas, e.g. "test (ubuntu-latest, 22); verify-pack".\n' +
        '  --strict       → also require the branch to be up to date with the base before\n' +
        '                   merging. Off by default: with slow CI it makes merging a race\n' +
        '                   against every other branch, and auto-merge cannot win it.\n' +
        '  --approvals 0  → PR required (plus any --checks), but the author/owner may merge\n' +
        '                   without a separate approval (for solo/small repos where Copilot\n' +
        '                   reviews but can\'t formally approve)');
}

// ── gh helpers ───────────────────────────────────────────────────────────────
function die(msg) {
    console.error(`✗ ${msg}`);
    process.exit(1);
}

function gh(args, { input } = {}) {
    const res = spawnSync('gh', args, { encoding: 'utf8', input });
    if (res.error) {
        if (res.error.code === 'ENOENT') die('GitHub CLI (`gh`) not found — install it and run `gh auth login`.');
        die(`gh failed: ${res.error.message}`);
    }
    return res;
}

function ghJson(args, { input, allowFail } = {}) {
    const res = gh(args, { input });
    if (res.status !== 0) {
        if (allowFail) return null;
        die(`gh ${args.join(' ')}\n${res.stderr || res.stdout}`);
    }
    return res.stdout.trim() ? JSON.parse(res.stdout) : {};
}

const [owner, name] = repo.split('/');

// ── 1. repo merge settings (squash-only + auto-delete) ───────────────────────
const repoSettings = {
    allow_squash_merge: true,
    // Auto-merge (`gh pr merge --auto`), so landing a PR is not a race the author
    // has to win by hand. Without it, the only way to merge is to catch the
    // window between "checks went green" and "someone else pushed to main" —
    // which, with CI in the minutes, is a window you lose more often than not.
    allow_auto_merge: true,
    // The "Update branch" button. Not required by anything here; it is the
    // manual escape hatch for the rare PR that genuinely does need rebasing.
    allow_update_branch: true,
    allow_merge_commit: false,
    allow_rebase_merge: false,
    delete_branch_on_merge: true,
    // PR title + body instead of the COMMIT_MESSAGES concatenation. Trailers are
    // NOT fully prevented here — GitHub appends Co-authored-by to any generated
    // message — so the merge step also passes --subject/--body (AGENTS.md step 6).
    squash_merge_commit_title: 'PR_TITLE',
    squash_merge_commit_message: 'PR_BODY',
};

// ── 2. the ruleset ───────────────────────────────────────────────────────────
const pullRequestRule = {
    type: 'pull_request',
    parameters: {
        required_approving_review_count: approvals,
        dismiss_stale_reviews_on_push: true,
        // Code-owner review implies an approval — only enforce it when approvals are required.
        require_code_owner_review: approvals > 0,
        require_last_push_approval: false,
        required_review_thread_resolution: true,
    },
};

const rules = [
    { type: 'deletion' },
    { type: 'non_fast_forward' }, // blocks force-push
    pullRequestRule,
];

if (checks.length) {
    rules.push({
        type: 'required_status_checks',
        parameters: {
            // Requiring an up-to-date branch sounds strictly safer and is not,
            // once CI takes minutes and more than one branch is in flight: every
            // merge to `main` invalidates every open PR, so each one rebases,
            // waits out CI again, and usually loses the race again. Auto-merge
            // cannot rescue it either — a stale branch never becomes mergeable
            // on its own.
            //
            // What it buys, and what turning it off costs: a PR is tested
            // against the `main` it branched from rather than the one it lands
            // on. Textual conflicts are still blocked; a *semantic* one is not
            // — rename a function in one PR, add a caller in another, both
            // green alone, `main` broken together. A merge queue is the way to
            // have both (it tests the real post-merge state and merges in
            // order), and it needs every required workflow to trigger on
            // `merge_group:` or PRs enqueue and never complete.
            strict_required_status_checks_policy: strict,
            required_status_checks: checks.map((context) => ({ context })),
        },
    });
}

const ruleset = {
    name: RULESET_NAME,
    target: 'branch',
    enforcement: 'active',
    conditions: { ref_name: { include: [`refs/heads/${DEFAULT_BRANCH}`], exclude: [] } },
    rules,
    // Empty bypass list: not even admins skip the PR flow. Add actor IDs here if
    // you ever need an escape hatch.
    bypass_actors: [],
};

// ── apply ────────────────────────────────────────────────────────────────────
console.log(`Repo:   ${repo}`);
console.log(`Branch: ${DEFAULT_BRANCH}`);
console.log(`Checks: ${checks.length ? checks.join(', ') : '(none — pass --checks to require CI green)'}`);
console.log(`Reviews: ${approvals} approving review(s)${approvals === 0 ? ' — PR required, owner may self-merge' : ', CODEOWNERS enforced'}`);
console.log(`Merges: squash-only, message = PR title + body, auto-delete branch on merge, auto-merge allowed`);
console.log(`Up-to-date: ${strict ? 'required (--strict)' : 'not required — a green PR does not go stale when main moves'}`);

if (dryRun) {
    console.log('\n--dry-run — would PATCH repo settings:');
    console.log(JSON.stringify(repoSettings, null, 2));
    console.log('\n--dry-run — would create/update ruleset:');
    console.log(JSON.stringify(ruleset, null, 2));
    process.exit(0);
}

// Verify auth before mutating, for a clear error rather than a cryptic API failure.
// Use `gh api user` (the active token) rather than `gh auth status`, which exits
// non-zero if any *other* configured account is broken even when the active one works.
if (gh(['api', 'user', '-q', '.login']).status !== 0) {
    die('Not authenticated — run `gh auth login` (need admin on the repo).');
}

// Merge settings.
ghJson(['api', '-X', 'PATCH', `repos/${owner}/${name}`, '--input', '-'], {
    input: JSON.stringify(repoSettings),
});
console.log('✓ Merge settings applied (squash-only, PR title + body, auto-delete).');

// Ruleset: find existing by name, then PUT (update) or POST (create) — idempotent.
const existing = ghJson(['api', `repos/${owner}/${name}/rulesets`, '--paginate'], { allowFail: true }) || [];
const found = Array.isArray(existing) ? existing.find((r) => r.name === RULESET_NAME) : null;

if (found) {
    ghJson(['api', '-X', 'PUT', `repos/${owner}/${name}/rulesets/${found.id}`, '--input', '-'], {
        input: JSON.stringify(ruleset),
    });
    console.log(`✓ Ruleset updated (id ${found.id}).`);
} else {
    const created = ghJson(['api', '-X', 'POST', `repos/${owner}/${name}/rulesets`, '--input', '-'], {
        input: JSON.stringify(ruleset),
    });
    console.log(`✓ Ruleset created (id ${created.id}).`);
}

console.log(`\n✓ '${DEFAULT_BRANCH}' is protected on ${repo}.`);
if (!checks.length) {
    console.log(
        '\nNext: require CI to be green before merge. Find your check names on an\n' +
        'open PR with `gh pr checks <pr>`, then re-run with e.g.\n' +
        `  node scripts/apply-branch-protection.mjs ${repo} --checks "test (ubuntu-latest, 22); verify-pack"`,
    );
}
