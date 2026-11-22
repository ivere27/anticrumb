//! Pure-Rust A/B testing & 4-arm ablation harness for `tree_sitter_diff` (no Python dependency).
//!
//! Subcommands:
//!   cargo run --locked -q -p xtask -- bench fixtures --out <dir>
//!   cargo run --locked -q -p xtask -- bench offline [--commits sha[@kib],...] [--fixtures-dir <dir>] [--arms A,B1,B2,B3] [--budget-kib 512] --out <dir>
//!   cargo run --locked -q -p xtask -- bench run-llm --dump-dir <dir> --out <dir> [--arms A,B3] [--runs 1] [--case-filter <sub] [--model <m>] [--thinking <t>] [--anticrumb-bin <bin>]
//!   cargo run --locked -q -p xtask -- bench grade --rubrics <rubrics.json> --runs-dir <dir>

use crate::next_flag_value;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const DEFAULT_ARMS: &str = "A,B1,B2,B3";

const REVIEW_PROMPT: &str = r#"You are performing a strict 1-turn code review using ONLY the attached patch file.
Do NOT assume or invent files outside the patch.
Find concrete bugs, contract/type/unit mismatches, null-safety issues, argument-order bugs, or missing edge-case guards introduced by this diff.
If you cannot verify something because an external definition or function header is missing from the patch, list that symbol in "missing_context" instead of guessing.

Output ONLY valid JSON matching this schema:
{
  "findings": [
    {
      "file": "path/to/file",
      "line": 123,
      "severity": "high|medium|low",
      "title": "short summary",
      "evidence": "exact symbol/type/line evidence from the patch",
      "explanation": "why this is a concrete bug"
    }
  ],
  "missing_context": ["symbol or file you needed but could not see in the patch"]
}"#;

pub fn run_cli(root: &Path, args: &[String]) -> i32 {
    let Some((subcmd, rest)) = args.split_first() else {
        print_usage();
        return 2;
    };
    let res = match subcmd.as_str() {
        "fixtures" => cmd_fixtures(rest),
        "offline" => cmd_offline(root, rest),
        "run-llm" => cmd_run_llm(rest),
        "grade" => cmd_grade(rest),
        "-h" | "--help" | "help" => {
            print_usage();
            return 0;
        }
        other => Err(format!("unknown bench subcommand: {other}")),
    };
    match res {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("xtask bench error: {err}");
            1
        }
    }
}

fn print_usage() {
    eprintln!(
        "\
Usage: cargo run --locked -q -p xtask -- bench <subcommand> [options]

Subcommands:
  fixtures  --out <dir>
  offline   --out <dir> [--commits <sha[@kib],...>] [--fixtures-dir <dir>] [--arms A,B1,B2,B3] [--budget-kib 512]
  run-llm   --dump-dir <dir> --out <dir> [--arms A,B3] [--runs 1] [--case-filter <str>] [--anticrumb-bin <bin>] [--model <m>] [--thinking <t>]
  grade     --rubrics <rubrics.json> --runs-dir <dir>"
    );
}

fn run_cmd(program: &str, args: &[&str], cwd: Option<&Path>) -> Result<String, String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("failed to run `{program}`: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "`{program} {}` exited with {}: {}",
            args.join(" "),
            out.status,
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn init_git_repo(
    repo_dir: &Path,
    initial_files: &[(&str, String)],
    modified_files: &[(&str, String)],
    commit_msg: &str,
) -> Result<String, String> {
    if repo_dir.exists() {
        fs::remove_dir_all(repo_dir).map_err(|e| format!("remove {}: {e}", repo_dir.display()))?;
    }
    fs::create_dir_all(repo_dir).map_err(|e| format!("mkdir {}: {e}", repo_dir.display()))?;

    run_cmd("git", &["init", "-q"], Some(repo_dir))?;
    run_cmd("git", &["config", "user.name", "bench"], Some(repo_dir))?;
    run_cmd(
        "git",
        &["config", "user.email", "bench@example.com"],
        Some(repo_dir),
    )?;
    run_cmd("git", &["config", "core.autocrlf", "false"], Some(repo_dir))?;

    for (rel, content) in initial_files {
        let p = repo_dir.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        fs::write(&p, content).map_err(|e| format!("write {}: {e}", p.display()))?;
    }
    run_cmd("git", &["add", "."], Some(repo_dir))?;
    run_cmd(
        "git",
        &["commit", "-q", "-m", "initial base"],
        Some(repo_dir),
    )?;

    for (rel, content) in modified_files {
        let p = repo_dir.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        fs::write(&p, content).map_err(|e| format!("write {}: {e}", p.display()))?;
    }
    run_cmd("git", &["add", "."], Some(repo_dir))?;
    run_cmd("git", &["commit", "-q", "-m", commit_msg], Some(repo_dir))?;
    let sha = run_cmd("git", &["rev-parse", "HEAD"], Some(repo_dir))?;
    Ok(sha.trim().to_string())
}

fn build_case1_rust_contracts(base_dir: &Path) -> Result<Value, String> {
    let settlement_rs = r#"//! Cross-file settlement primitives (unmodified in target commit).

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reservation {
    pub reservation_id: String,
    pub held_cents: u64,
    pub expires_after_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettlementError {
    InsufficientFunds { available_cents: u64, requested_cents: u64 },
    DuplicateIdempotencyKey,
    TimeoutTooShort { min_ms: u64 },
}

/// Reserves funds in **cents** (NOT whole dollars!).
/// `timeout_ms` must be in milliseconds (minimum 1000 ms).
pub fn reserve_funds(
    account_id: u64,
    amount_cents: u64,
    timeout_ms: u64,
    idempotency_key: &str,
) -> Result<Reservation, SettlementError> {
    if timeout_ms < 1000 {
        return Err(SettlementError::TimeoutTooShort { min_ms: 1000 });
    }
    Ok(Reservation {
        reservation_id: format!("res-{account_id}-{idempotency_key}"),
        held_cents: amount_cents,
        expires_after_ms: timeout_ms,
    })
}
"#
    .to_string();

    let long_helper_body = (1..55)
        .map(|i| format!("    let step_{i} = acc.wrapping_add({i} * 7); acc = step_{i};"))
        .collect::<Vec<_>>()
        .join("\n");

    let ledger_base = format!(
        r#"use crate::settlement::reserve_funds;

/// Fee basis-points denominator: 100 bps = 1%, 10_000 bps = 100%.
pub const BPS_DENOMINATOR: u64 = 10_000;
/// Maximum allowed single transfer in cents ($10,000.00).
pub const MAX_TRANSFER_CENTS: u64 = 1_000_000;

#[derive(Clone, Debug)]
pub struct LedgerPolicy {{
    pub account_id: u64,
    /// Fee rate expressed in basis points (e.g., 250 = 2.50%).
    pub fee_bps: u64,
    /// Whether settlement holds must include the computed fee in `amount_cents`.
    pub hold_includes_fee: bool,
}}

fn compute_audit_checksum(seed: u64) -> u64 {{
    let mut acc = seed;
{long_helper_body}
    acc
}}

pub fn settle_payout(policy: &LedgerPolicy, whole_dollars: u64, timeout_secs: u64, req_id: &str) -> Result<u64, String> {{
    let gross_cents = whole_dollars * 100;
    let fee_cents = (gross_cents * policy.fee_bps) / BPS_DENOMINATOR;
    let hold = if policy.hold_includes_fee {{ gross_cents + fee_cents }} else {{ gross_cents }};
    let res = reserve_funds(policy.account_id, hold, timeout_secs * 1000, req_id)
        .map_err(|e| format!("settlement failed: {{e:?}}"))?;
    Ok(res.held_cents)
}}
"#
    );

    let ledger_mod = format!(
        r#"use crate::settlement::reserve_funds;

/// Fee basis-points denominator: 100 bps = 1%, 10_000 bps = 100%.
pub const BPS_DENOMINATOR: u64 = 10_000;
/// Maximum allowed single transfer in cents ($10,000.00).
pub const MAX_TRANSFER_CENTS: u64 = 1_000_000;

#[derive(Clone, Debug)]
pub struct LedgerPolicy {{
    pub account_id: u64,
    /// Fee rate expressed in basis points (e.g., 250 = 2.50%).
    pub fee_bps: u64,
    /// Whether settlement holds must include the computed fee in `amount_cents`.
    pub hold_includes_fee: bool,
}}

pub(crate) fn compute_audit_checksum(seed: u64) -> u64 {{
    let mut acc = seed;
{long_helper_body}
    acc
}}

pub fn settle_payout(policy: &LedgerPolicy, whole_dollars: u64, timeout_secs: u64, req_id: &str) -> Result<u64, String> {{
    // Refactored payout settlement path
    let gross_cents = whole_dollars * 100;
    let fee_cents = (gross_cents * policy.fee_bps) / 100;
    let hold_dollars = (gross_cents + fee_cents) / 100;
    let res = reserve_funds(policy.account_id, hold_dollars, timeout_secs, req_id)
        .map_err(|e| format!("settlement failed: {{e:?}}"))?;
    Ok(res.held_cents)
}}
"#
    );

    let repo_dir = base_dir.join("case1_rust_contracts");
    let sha = init_git_repo(
        &repo_dir,
        &[
            (
                "src/lib.rs",
                "pub mod settlement;\npub mod ledger;\n".to_string(),
            ),
            ("src/settlement.rs", settlement_rs),
            ("src/ledger.rs", ledger_base),
        ],
        &[("src/ledger.rs", ledger_mod)],
        "feat(ledger): expose checksum helper and simplify payout hold",
    )?;
    Ok(json!({
        "id": "case1_rust_contracts",
        "repo": repo_dir.display().to_string(),
        "source": format!("commit:{sha}"),
        "languages": ["Rust"],
        "tests_hypotheses": ["layer2_sig_only", "layer3_local", "layer3_cross"],
        "expected_bugs": [
            {
                "id": "B1_bps_div_100",
                "file": "src/ledger.rs",
                "keywords": ["fee_bps", "BPS_DENOMINATOR", "10_000", "10000", "basis points", "100"],
                "description": "Divides fee_bps (basis points, /10_000) by 100 instead of BPS_DENOMINATOR (100x overcharge), and ignores policy.hold_includes_fee.",
                "needs_layer": "layer3_local"
            },
            {
                "id": "B2_reserve_funds_units",
                "file": "src/ledger.rs",
                "keywords": ["reserve_funds", "amount_cents", "hold_dollars", "cents", "timeout_ms", "timeout_secs", "1000"],
                "description": "Passes whole dollars (`hold_dollars`) to `reserve_funds(..., amount_cents, ...)` AND passes `timeout_secs` directly as `timeout_ms` without `* 1000`.",
                "needs_layer": "layer3_cross"
            }
        ]
    }))
}

fn build_case2_ts_py_clamped_cross(base_dir: &Path) -> Result<Value, String> {
    let filler_ts = (1..215)
        .map(|i| {
            format!(
                "  subtotal += entries[{i} % Math.max(1, entries.length)]?.amountCents ?? 0; // step {i}"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let ts_base = format!(
        r#"export interface InvoiceEntry {{
  id: string;
  amountCents: number;
  credit: boolean;
}}

export interface AuditSink {{
  record(tenantId: string, deltaCents: number): void;
}}

export interface ReconcileOptions {{
  strictMode: boolean;
  maxEntries: number;
}}

export function reconcileBatch(
  tenantId: string,
  entries: InvoiceEntry[],
  options: ReconcileOptions,
  allowNegativeBalance: boolean,
  auditWriter: AuditSink | null
): number {{
  let subtotal = 0;
{filler_ts}
  if (!allowNegativeBalance && subtotal < 0) {{
    throw new Error("negative balance disallowed");
  }}
  if (auditWriter !== null) {{
    auditWriter.record(tenantId, subtotal);
  }}
  return subtotal;
}}
"#
    );

    let ts_mod = ts_base.replace(
        "  if (!allowNegativeBalance && subtotal < 0) {\n    throw new Error(\"negative balance disallowed\");\n  }\n  if (auditWriter !== null) {\n    auditWriter.record(tenantId, subtotal);\n  }",
        "  // Fast-path audit logging for batch reconciliation\n  auditWriter.record(tenantId, subtotal);",
    );

    let py_tax = r#"from dataclasses import dataclass

@dataclass(frozen=True)
class TaxBreakdown:
    net_cents: int
    tax_cents: int
    gross_cents: int


def compute_vat(amount_cents: int, rate_bps: int) -> tuple[int, int]:
    """Returns (net_cents, tax_cents) tuple."""
    tax = (amount_cents * rate_bps) // 10_000
    return (amount_cents, tax)
"#
    .to_string();

    let py_checkout_base = r#"from pricing.tax import compute_vat


class CheckoutService:
    DEFAULT_VAT_BPS = 1000

    @classmethod
    def quote_total(cls, subtotal_cents: int) -> int:
        net_cents, tax_cents = compute_vat(subtotal_cents, cls.DEFAULT_VAT_BPS)
        return net_cents + tax_cents
"#
    .to_string();

    let py_checkout_mod = r#"from pricing.tax import compute_vat


class CheckoutService:
    DEFAULT_VAT_BPS = 1000

    @classmethod
    def quote_total(cls, subtotal_cents: int) -> int:
        tax_cents, gross_cents = compute_vat(subtotal_cents, cls.DEFAULT_VAT_BPS)
        return gross_cents + tax_cents
"#
    .to_string();

    let repo_dir = base_dir.join("case2_ts_py_clamped_cross");
    let sha = init_git_repo(
        &repo_dir,
        &[
            ("src/reconcile.ts", ts_base),
            ("pricing/__init__.py", String::new()),
            ("pricing/tax.py", py_tax),
            ("pricing/checkout.py", py_checkout_base),
        ],
        &[
            ("src/reconcile.ts", ts_mod),
            ("pricing/checkout.py", py_checkout_mod),
        ],
        "perf: streamline reconciliation audit and checkout quote",
    )?;
    Ok(json!({
        "id": "case2_ts_py_clamped_cross",
        "repo": repo_dir.display().to_string(),
        "source": format!("commit:{sha}"),
        "languages": ["TypeScript", "Python"],
        "tests_hypotheses": ["layer2_clamped_params", "layer2_decorators", "layer3_cross"],
        "expected_bugs": [
            {
                "id": "B1_ts_nullable_auditWriter_and_negative_guard",
                "file": "src/reconcile.ts",
                "keywords": ["auditWriter", "null", "allowNegativeBalance", "negative"],
                "description": "Calls `auditWriter.record(...)` without null check even though `auditWriter: AuditSink | null` is nullable, and removes the `allowNegativeBalance` guard.",
                "needs_layer": "layer2_clamped_params"
            },
            {
                "id": "B2_py_compute_vat_tuple_unpack",
                "file": "pricing/checkout.py",
                "keywords": ["compute_vat", "net_cents", "tax_cents", "gross_cents", "tuple", "order", "unpack"],
                "description": "`compute_vat` returns `(net_cents, tax_cents)`, NOT `(tax_cents, gross_cents)`.",
                "needs_layer": "layer3_cross"
            }
        ]
    }))
}

fn build_case3_go_java_multilang(base_dir: &Path) -> Result<Value, String> {
    let go_queue = r#"package jobs

import "context"

type JobConfig struct {
	MaxRetries int
	TimeoutSec int // Must be > 0; 0 causes immediate deadline cancellation
}

// EnqueueJob schedules a job. Parameter order: (ctx, queueName, payload, cfg).
// Returns ("", nil) if the job was deduplicated (NOT an error, empty ID).
func EnqueueJob(ctx context.Context, queueName string, payload []byte, cfg JobConfig) (string, error) {
	if cfg.TimeoutSec <= 0 {
		return "", context.DeadlineExceeded
	}
	return "job-1", nil
}
"#
    .to_string();

    let go_worker_base = r#"package jobs

import "context"

func DispatchEmail(ctx context.Context, body []byte) (string, error) {
	cfg := JobConfig{MaxRetries: 3, TimeoutSec: 30}
	id, err := EnqueueJob(ctx, "email", body, cfg)
	if err != nil {
		return "", err
	}
	return id, nil
}
"#
    .to_string();

    let go_worker_mod = r#"package jobs

import "context"

func DispatchEmail(ctx context.Context, body []byte) (string, error) {
	cfg := JobConfig{MaxRetries: 3}
	id, err := EnqueueJob(ctx, string(body), []byte("email"), cfg)
	if err != nil {
		return "", err
	}
	return id, nil
}
"#
    .to_string();

    let java_inv = r#"package com.acme.orders;

public class InventoryService {
    /**
     * Reserves inventory. Note parameter order: (warehouseId, sku, quantity).
     */
    public boolean reserveStock(String warehouseId, String sku, int quantity) {
        return quantity > 0 && warehouseId != null && sku != null;
    }
}
"#
    .to_string();

    let java_noise_body = (1..38)
        .map(|i| format!("        total += step * {i};"))
        .collect::<Vec<_>>()
        .join("\n");

    let java_ctrl_base = format!(
        r#"package com.acme.orders;

public class OrderController {{
    private final InventoryService inventory;

    public OrderController(InventoryService inventory) {{
        this.inventory = inventory;
    }}

    protected long computeMetrics(long step) {{
        long total = 0;
{java_noise_body}
        return total;
    }}

    @Transactional
    public boolean placeOrder(String sku, int qty, String warehouseId) {{
        return inventory.reserveStock(warehouseId, sku, qty);
    }}
}}
"#
    );

    let java_ctrl_mod = java_ctrl_base
        .replace(
            "    protected long computeMetrics(long step) {",
            "    public long computeMetrics(long step) {",
        )
        .replace(
            "        return inventory.reserveStock(warehouseId, sku, qty);",
            "        return inventory.reserveStock(sku, warehouseId, qty);",
        );

    let repo_dir = base_dir.join("case3_go_java_multilang");
    let sha = init_git_repo(
        &repo_dir,
        &[
            ("jobs/queue.go", go_queue),
            ("jobs/worker.go", go_worker_base),
            (
                "src/main/java/com/acme/orders/InventoryService.java",
                java_inv,
            ),
            (
                "src/main/java/com/acme/orders/OrderController.java",
                java_ctrl_base,
            ),
        ],
        &[
            ("jobs/worker.go", go_worker_mod),
            (
                "src/main/java/com/acme/orders/OrderController.java",
                java_ctrl_mod,
            ),
        ],
        "refactor(jobs,orders): simplify dispatch and expose metrics",
    )?;
    Ok(json!({
        "id": "case3_go_java_multilang",
        "repo": repo_dir.display().to_string(),
        "source": format!("commit:{sha}"),
        "languages": ["Go", "Java"],
        "tests_hypotheses": ["layer1_toc", "layer2_sig_only", "layer2_decorators", "layer3_cross"],
        "expected_bugs": [
            {
                "id": "B1_go_enqueue_args_and_timeout",
                "file": "jobs/worker.go",
                "keywords": ["TimeoutSec", "EnqueueJob", "queueName", "payload", "swapped", "0"],
                "description": "Omits `TimeoutSec` (defaults to 0 -> immediate DeadlineExceeded) AND swaps `queueName` and `payload` arguments to `EnqueueJob`.",
                "needs_layer": "layer3_cross"
            },
            {
                "id": "B2_java_reserveStock_arg_swap",
                "file": "src/main/java/com/acme/orders/OrderController.java",
                "keywords": ["reserveStock", "warehouseId", "sku", "swap", "order"],
                "description": "Passes `(sku, warehouseId, qty)` to `InventoryService.reserveStock(String warehouseId, String sku, int quantity)` with swapped `warehouseId` and `sku`.",
                "needs_layer": "layer3_cross"
            }
        ]
    }))
}

fn build_case4_cs_cpp_ruby(base_dir: &Path) -> Result<Value, String> {
    let cpp_hdr = r#"#pragma once
#include <cstddef>
#include <cstdint>

class RingBuffer {
public:
    // Writes up to `len` bytes. Returns actual bytes written (may be < len if full!).
    size_t write(const uint8_t* data, size_t len);
    size_t available_space() const;
};
"#
    .to_string();

    let cpp_src_base = r#"#include "ring_buffer.hpp"

// 한글 스트림 전송 헬퍼 (UTF-8 경계 검증)
bool send_frame(RingBuffer& rb, const uint8_t* payload, size_t len) {
    size_t written = rb.write(payload, len);
    return written == len;
}
"#
    .to_string();

    let cpp_src_mod = r#"#include "ring_buffer.hpp"

// 한글 스트림 전송 헬퍼 (UTF-8 경계 검증)
bool send_frame(RingBuffer& rb, const uint8_t* payload, size_t len) {
    // Assume write returns non-zero on complete success
    return rb.write(payload, len) > 0;
}
"#
    .to_string();

    let cs_policy = r#"namespace Billing.Policies;

public sealed record RefundPolicy(long MaxRefundCents, bool RequireManagerApproval);

public static class RefundLimits {
    public const long MAX_AUTO_REFUND_CENTS = 50_000;
    public static bool CanAutoRefund(long amountCents, RefundPolicy policy) =>
        amountCents > 0 && amountCents <= policy.MaxRefundCents && !policy.RequireManagerApproval;
}
"#
    .to_string();

    let cs_ctrl_base = r#"using Billing.Policies;

namespace Billing.Controllers;

public class RefundController {
    [Authorize]
    [HttpPost("/refunds")]
    public bool ProcessRefund(long amountCents, RefundPolicy policy) {
        return RefundLimits.CanAutoRefund(amountCents, policy);
    }
}
"#
    .to_string();

    let cs_ctrl_mod = r#"using Billing.Policies;

namespace Billing.Controllers;

public class RefundController {
    [HttpPost("/refunds")]
    public bool ProcessRefund(long amountCents, RefundPolicy policy) {
        return amountCents <= policy.MaxRefundCents;
    }
}
"#
    .to_string();

    let rb_limiter = r#"class RateLimiter
  # Checks rate limit for `key`. Keyword argument is `cost:` (NOT `weight:`).
  def self.allow?(key, cost: 1)
    cost <= 10 && !key.empty?
  end
end
"#
    .to_string();

    let rb_handler_base = r#"require_relative 'rate_limiter'

class ApiHandler
  def handle_request(api_key, units)
    return :throttled unless RateLimiter.allow?(api_key, cost: units)
    :ok
  end
end
"#
    .to_string();

    let rb_handler_mod = r#"require_relative 'rate_limiter'

class ApiHandler
  def handle_request(api_key, units)
    return :throttled unless RateLimiter.allow?(api_key, weight: units)
    :ok
  end
end
"#
    .to_string();

    let repo_dir = base_dir.join("case4_cs_cpp_ruby");
    let sha = init_git_repo(
        &repo_dir,
        &[
            ("include/ring_buffer.hpp", cpp_hdr),
            ("src/stream.cpp", cpp_src_base),
            ("Policies/RefundPolicy.cs", cs_policy),
            ("Controllers/RefundController.cs", cs_ctrl_base),
            ("lib/rate_limiter.rb", rb_limiter),
            ("lib/api_handler.rb", rb_handler_base),
        ],
        &[
            ("src/stream.cpp", cpp_src_mod),
            ("Controllers/RefundController.cs", cs_ctrl_mod),
            ("lib/api_handler.rb", rb_handler_mod),
        ],
        "chore: simplify stream write, refund check, and rate limit call",
    )?;
    Ok(json!({
        "id": "case4_cs_cpp_ruby",
        "repo": repo_dir.display().to_string(),
        "source": format!("commit:{sha}"),
        "languages": ["C++", "C#", "Ruby"],
        "tests_hypotheses": ["layer2_decorators", "layer2_utf8", "layer3_cross"],
        "expected_bugs": [
            {
                "id": "B1_cpp_partial_write",
                "file": "src/stream.cpp",
                "keywords": ["write", "partial", "written == len", "> 0", "len"],
                "description": "`RingBuffer::write` returns bytes written (can be partial < len); checking `> 0` falsely reports success on truncated writes.",
                "needs_layer": "layer3_cross"
            },
            {
                "id": "B2_cs_refund_policy_and_authorize",
                "file": "Controllers/RefundController.cs",
                "keywords": ["Authorize", "RequireManagerApproval", "amountCents <= 0", "negative", "CanAutoRefund"],
                "description": "Removed `[Authorize]` attribute, bypassed `policy.RequireManagerApproval`, and dropped `amountCents > 0` guard.",
                "needs_layer": "layer3_cross"
            },
            {
                "id": "B3_rb_wrong_kwarg",
                "file": "lib/api_handler.rb",
                "keywords": ["weight:", "cost:", "RateLimiter", "allow?", "ArgumentError", "keyword"],
                "description": "Calls `RateLimiter.allow?(api_key, weight: units)` but `RateLimiter.allow?` defines `cost:`, raising `ArgumentError`.",
                "needs_layer": "layer3_cross"
            }
        ]
    }))
}

fn build_case5_negative_control(base_dir: &Path) -> Result<Value, String> {
    let filler = (1..34)
        .map(|i| format!("    let _v{i} = base.saturating_add({i});"))
        .collect::<Vec<_>>()
        .join("\n");
    let clean_base = format!(
        r#"pub fn format_label(prefix: &str, count: usize) -> String {{
    let trimmed = prefix.trim();
    format!("{{trimmed}}:{{count}}")
}}

fn stable_report(base: u64) -> u64 {{
{filler}
    base
}}
"#
    );
    let clean_mod = format!(
        r#"pub fn format_label(prefix: &str, count: usize) -> String {{
    let label = prefix.trim();
    format!("{{label}}:{{count}}")
}}

pub(crate) fn stable_report(base: u64) -> u64 {{
{filler}
    base
}}
"#
    );

    let discount_filler = (1..30)
        .map(|i| format!("    let _audit_{i} = price.saturating_sub({i});"))
        .collect::<Vec<_>>()
        .join("\n");
    let discount_base = format!(
        r#"pub fn calc_discount(price: u64, pct: u8) -> u64 {{
    let capped = pct.min(100);
{discount_filler}
    price.saturating_mul(capped as u64) / 100
}}
"#
    );
    let discount_mod = format!(
        r#"pub fn calc_discount(price: u64, pct: i32) -> u64 {{
    let capped = pct.min(100);
{discount_filler}
    price.saturating_mul(capped as u64) / 100
}}
"#
    );

    let repo_dir = base_dir.join("case5_negative_control");
    let sha = init_git_repo(
        &repo_dir,
        &[
            ("src/clean.rs", clean_base),
            ("src/discount.rs", discount_base),
        ],
        &[
            ("src/clean.rs", clean_mod),
            ("src/discount.rs", discount_mod),
        ],
        "refactor: expose report helper and widen discount pct to i32",
    )?;
    Ok(json!({
        "id": "case5_negative_control",
        "repo": repo_dir.display().to_string(),
        "source": format!("commit:{sha}"),
        "languages": ["Rust"],
        "tests_hypotheses": ["false_positive_control", "layer2_sig_only_risk"],
        "expected_bugs": [
            {
                "id": "B1_negative_i32_discount",
                "file": "src/discount.rs",
                "keywords": ["i32", "negative", "as u64", "min(100)", "clamp", "wrap"],
                "description": "Widened `pct: u8` to signed `i32` without clamping lower bound (`pct < 0`); negative `pct` casts via `as u64` to huge unsigned value.",
                "needs_layer": "layer2_sig_only_risk_check"
            }
        ],
        "clean_files": ["src/clean.rs"]
    }))
}

fn cmd_fixtures(args: &[String]) -> Result<(), String> {
    let mut out_dir: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => out_dir = Some(PathBuf::from(next_flag_value(args, &mut i, "--out")?)),
            other => return Err(format!("unknown flag for fixtures: {other}")),
        }
    }
    let out_dir = out_dir.ok_or("--out is required")?;
    fs::create_dir_all(&out_dir).map_err(|e| format!("mkdir {}: {e}", out_dir.display()))?;

    let cases = vec![
        build_case1_rust_contracts(&out_dir)?,
        build_case2_ts_py_clamped_cross(&out_dir)?,
        build_case3_go_java_multilang(&out_dir)?,
        build_case4_cs_cpp_ruby(&out_dir)?,
        build_case5_negative_control(&out_dir)?,
    ];
    let rubrics_path = out_dir.join("rubrics.json");
    let body = serde_json::to_string_pretty(&json!({ "cases": cases }))
        .map_err(|e| format!("serialize rubrics: {e}"))?;
    fs::write(&rubrics_path, body).map_err(|e| format!("write rubrics: {e}"))?;
    println!(
        "Generated {} multi-language fixture repos + {}",
        cases.len(),
        rubrics_path.display()
    );
    Ok(())
}

fn ensure_bench_binary(root: &Path) -> Result<PathBuf, String> {
    let stdout = run_cmd(
        "cargo",
        &[
            "build",
            "--release",
            "--example",
            "diff_ab_bench",
            "--message-format=json",
        ],
        Some(root),
    )?;
    for line in stdout.lines() {
        if let Ok(val) = serde_json::from_str::<Value>(line) {
            if val["reason"].as_str() == Some("compiler-artifact")
                && val["target"]["name"].as_str() == Some("diff_ab_bench")
            {
                if let Some(exe) = val["executable"].as_str() {
                    let p = PathBuf::from(exe);
                    if p.exists() {
                        return Ok(p);
                    }
                }
            }
        }
    }
    let target_dir = match std::env::var_os("CARGO_TARGET_DIR") {
        Some(v) => {
            let p = PathBuf::from(v);
            if p.is_absolute() {
                p
            } else {
                root.join(p)
            }
        }
        None => root.join("target"),
    };
    let bin_name = if cfg!(windows) {
        "diff_ab_bench.exe"
    } else {
        "diff_ab_bench"
    };
    let bin = target_dir.join("release/examples").join(bin_name);
    if !bin.exists() {
        return Err(format!("expected example binary at {}", bin.display()));
    }
    Ok(bin)
}

fn fmt_commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (idx, ch) in s.chars().enumerate() {
        if idx > 0 && (s.len() - idx).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn cmd_offline(root: &Path, args: &[String]) -> Result<(), String> {
    let mut out_dir: Option<PathBuf> = None;
    let mut commits_arg = String::new();
    let mut fixtures_dir: Option<PathBuf> = None;
    let mut arms_arg = DEFAULT_ARMS.to_string();
    let mut budget_kib: usize = 512;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => out_dir = Some(PathBuf::from(next_flag_value(args, &mut i, "--out")?)),
            "--commits" => commits_arg = next_flag_value(args, &mut i, "--commits")?.to_string(),
            "--fixtures-dir" => {
                fixtures_dir = Some(PathBuf::from(next_flag_value(
                    args,
                    &mut i,
                    "--fixtures-dir",
                )?))
            }
            "--arms" => arms_arg = next_flag_value(args, &mut i, "--arms")?.to_string(),
            "--budget-kib" => {
                let raw = next_flag_value(args, &mut i, "--budget-kib")?;
                budget_kib = raw
                    .parse()
                    .map_err(|_| format!("invalid --budget-kib: {raw}"))?;
            }
            other => return Err(format!("unknown flag for offline: {other}")),
        }
    }
    let out_dir = out_dir.ok_or("--out is required")?;
    fs::create_dir_all(out_dir.join("dumps")).map_err(|e| format!("mkdir dumps: {e}"))?;

    let arms: Vec<String> = arms_arg
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    let bin = ensure_bench_binary(root)?;

    struct Target {
        id: String,
        repo: PathBuf,
        source: String,
        budget_kib: usize,
    }
    let mut targets: Vec<Target> = Vec::new();
    for item in commits_arg
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let (sha, bk) = match item.split_once('@') {
            Some((s, b)) => (
                s,
                b.parse::<usize>()
                    .map_err(|_| format!("invalid commit budget: {item}"))?,
            ),
            None => (item, budget_kib),
        };
        let short: String = sha.chars().take(7).collect();
        targets.push(Target {
            id: format!("real_{short}_{bk}k"),
            repo: root.to_path_buf(),
            source: format!("commit:{sha}"),
            budget_kib: bk,
        });
    }

    if let Some(fdir) = fixtures_dir {
        let rtext = fs::read_to_string(fdir.join("rubrics.json"))
            .map_err(|e| format!("read rubrics.json: {e}"))?;
        let rubrics: Value =
            serde_json::from_str(&rtext).map_err(|e| format!("parse rubrics.json: {e}"))?;
        if let Some(cases) = rubrics["cases"].as_array() {
            for c in cases {
                targets.push(Target {
                    id: c["id"].as_str().unwrap_or("case").to_string(),
                    repo: PathBuf::from(c["repo"].as_str().unwrap_or(".")),
                    source: c["source"].as_str().unwrap_or("diff").to_string(),
                    budget_kib,
                });
            }
        }
    }

    let mut rows: Vec<Value> = Vec::new();
    for t in &targets {
        for arm in &arms {
            let dump_file = out_dir.join("dumps").join(format!("{}_{arm}.patch", t.id));
            let repo_str = t.repo.to_string_lossy().into_owned();
            let bk_str = t.budget_kib.to_string();
            let dump_str = dump_file.to_string_lossy().into_owned();
            let stdout = run_cmd(
                &bin.to_string_lossy(),
                &[
                    "--repo",
                    &repo_str,
                    "--source",
                    &t.source,
                    "--arm",
                    arm,
                    "--budget-kib",
                    &bk_str,
                    "--out",
                    &dump_str,
                ],
                None,
            )?;
            let mut m: Value =
                serde_json::from_str(&stdout).map_err(|e| format!("parse metrics JSON: {e}"))?;
            if let Some(obj) = m.as_object_mut() {
                obj.insert("case_id".to_string(), json!(t.id));
                obj.insert("dump_file".to_string(), json!(dump_str));
            }
            rows.push(m);
        }
    }

    let report_json = out_dir.join("offline_report.json");
    fs::write(
        &report_json,
        serde_json::to_string_pretty(&rows).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("write offline_report.json: {e}"))?;

    let mut md = String::from(
        "# Offline 4-Arm Ablation Report\n\n\
         | Case | Arm | Total B | Est Tok | L1 B | L2 B | L3 B | L3 (loc/x) | SigOnly | Clamped | More(+N/Named) | Compaction (U3/Skel/Trunc/Omit) | Latency ms |\n\
         |---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|\n",
    );
    for r in &rows {
        let comp = format!(
            "{}/{}/{}/{}",
            r["degraded_u3_files"].as_u64().unwrap_or(0),
            r["skeletonized_files"].as_u64().unwrap_or(0),
            r["truncated_files"].as_u64().unwrap_or(0),
            r["budget_omitted_files"].as_u64().unwrap_or(0),
        );
        md.push_str(&format!(
            "| `{}` | **{}** | {} | {} | {} | {} | {} | {}/{} | {} | {} | {}/{} | {} | {:.1} |\n",
            r["case_id"].as_str().unwrap_or(""),
            r["arm"].as_str().unwrap_or(""),
            fmt_commas(r["total_bytes"].as_u64().unwrap_or(0)),
            fmt_commas(r["est_tokens_div4"].as_u64().unwrap_or(0)),
            fmt_commas(r["layer1_bytes"].as_u64().unwrap_or(0)),
            fmt_commas(r["layer2_bytes"].as_u64().unwrap_or(0)),
            fmt_commas(r["layer3_bytes"].as_u64().unwrap_or(0)),
            r["layer3_local_entries"].as_u64().unwrap_or(0),
            r["layer3_cross_entries"].as_u64().unwrap_or(0),
            r["sig_only_hunks"].as_u64().unwrap_or(0),
            r["clamped_hunks"].as_u64().unwrap_or(0),
            r["opaque_more_hunks"].as_u64().unwrap_or(0),
            r["named_more_hunks"].as_u64().unwrap_or(0),
            comp,
            r["latency_ms"].as_f64().unwrap_or(0.0),
        ));
    }

    let report_md = out_dir.join("offline_report.md");
    fs::write(&report_md, &md).map_err(|e| format!("write offline_report.md: {e}"))?;
    print!("{md}");
    println!(
        "\nSaved reports to {} and {}",
        report_md.display(),
        report_json.display()
    );
    Ok(())
}

fn cmd_run_llm(args: &[String]) -> Result<(), String> {
    let mut dump_dir: Option<PathBuf> = None;
    let mut out_dir: Option<PathBuf> = None;
    let mut arms_arg = String::from("A,B3");
    let mut runs: usize = 1;
    let mut case_filter = String::new();
    let mut anticrumb_bin = String::from("anticrumb");
    let mut model = String::new();
    let mut thinking = String::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dump-dir" => {
                dump_dir = Some(PathBuf::from(next_flag_value(args, &mut i, "--dump-dir")?))
            }
            "--out" => out_dir = Some(PathBuf::from(next_flag_value(args, &mut i, "--out")?)),
            "--arms" => arms_arg = next_flag_value(args, &mut i, "--arms")?.to_string(),
            "--runs" => {
                let raw = next_flag_value(args, &mut i, "--runs")?;
                runs = raw.parse().map_err(|_| format!("invalid --runs: {raw}"))?;
            }
            "--case-filter" => {
                case_filter = next_flag_value(args, &mut i, "--case-filter")?.to_string()
            }
            "--anticrumb-bin" => {
                anticrumb_bin = next_flag_value(args, &mut i, "--anticrumb-bin")?.to_string()
            }
            "--model" => model = next_flag_value(args, &mut i, "--model")?.to_string(),
            "--thinking" => thinking = next_flag_value(args, &mut i, "--thinking")?.to_string(),
            other => return Err(format!("unknown flag for run-llm: {other}")),
        }
    }

    let dump_dir = dump_dir.ok_or("--dump-dir is required")?;
    let out_dir = out_dir.ok_or("--out is required")?;
    fs::create_dir_all(&out_dir).map_err(|e| format!("mkdir {}: {e}", out_dir.display()))?;

    let arms: Vec<String> = arms_arg
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    let mut patches: Vec<PathBuf> = fs::read_dir(dump_dir.join("dumps"))
        .map_err(|e| format!("read dumps dir: {e}"))?
        .filter_map(|e| e.ok().map(|de| de.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("patch"))
        .collect();
    patches.sort();

    let mut selected = Vec::new();
    for p in patches {
        let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        for arm in &arms {
            let suffix = format!("_{arm}");
            if let Some(case_id) = stem.strip_suffix(&suffix) {
                if case_filter.is_empty() || case_id.contains(&case_filter) {
                    selected.push((case_id.to_string(), arm.clone(), p.clone()));
                }
            }
        }
    }

    let mut index = Vec::new();
    for (case_id, arm, patch_path) in selected {
        for run_idx in 1..=runs {
            eprintln!("[run {run_idx}/{runs}] {case_id} ({arm}) ...");
            let mut cmd = Command::new(&anticrumb_bin);
            cmd.args(["exec", "--chat", "--file"]).arg(&patch_path);
            if !model.is_empty() {
                cmd.args(["-m", &model]);
            }
            if !thinking.is_empty() {
                cmd.args(["--thinking", &thinking]);
            }
            cmd.arg(REVIEW_PROMPT);
            let out = cmd
                .output()
                .map_err(|e| format!("failed to run `{anticrumb_bin}`: {e}"))?;
            let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if !out.status.success() && !stderr.is_empty() {
                eprintln!("  [warn] {case_id} ({arm}) stderr: {stderr}");
            }
            let out_file = out_dir.join(format!("{case_id}_{arm}_run{run_idx}.json"));
            fs::write(&out_file, &raw).map_err(|e| format!("write {}: {e}", out_file.display()))?;
            index.push(json!({
                "case_id": case_id,
                "arm": arm,
                "run": run_idx,
                "returncode": out.status.code().unwrap_or(-1),
                "stderr": stderr,
                "output_file": out_file.display().to_string(),
            }));
        }
    }

    fs::write(
        out_dir.join("llm_runs_index.json"),
        serde_json::to_string_pretty(&index).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("write llm_runs_index.json: {e}"))?;
    println!(
        "Completed {} 1-turn LLM review runs in {}",
        index.len(),
        out_dir.display()
    );
    Ok(())
}

fn extract_json_payload(text: &str) -> Option<Value> {
    let s = text.trim();
    if s.is_empty() {
        return None;
    }
    let is_valid_review = |v: &Value| v.get("findings").and_then(Value::as_array).is_some();
    if s.contains("```") {
        for part in s.split("```") {
            let mut cand = part.trim();
            if let Some(rest) = cand.strip_prefix("json") {
                cand = rest.trim();
            }
            if let (Some(start), Some(end)) = (cand.find('{'), cand.rfind('}')) {
                if end > start {
                    if let Ok(v) = serde_json::from_str::<Value>(&cand[start..=end]) {
                        if is_valid_review(&v) {
                            return Some(v);
                        }
                    }
                }
            }
        }
    }
    if let (Some(start), Some(end)) = (s.find('{'), s.rfind('}')) {
        if end > start {
            if let Ok(v) = serde_json::from_str::<Value>(&s[start..=end]) {
                if is_valid_review(&v) {
                    return Some(v);
                }
            }
        }
    }
    None
}

struct RunIndexMeta {
    case_id: String,
    arm: String,
    run_str: String,
    returncode: i64,
}

fn load_run_index_meta(runs_dir: &Path) -> HashMap<String, RunIndexMeta> {
    let mut map = HashMap::new();
    let idx_path = runs_dir.join("llm_runs_index.json");
    let Ok(raw) = fs::read_to_string(idx_path) else {
        return map;
    };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&raw) else {
        return map;
    };
    for item in items {
        let Some(case_id) = item["case_id"].as_str() else {
            continue;
        };
        let Some(arm) = item["arm"].as_str() else {
            continue;
        };
        let Some(out_file) = item["output_file"].as_str() else {
            continue;
        };
        let Some(fname) = Path::new(out_file).file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let run_str = item["run"]
            .as_u64()
            .map(|r| r.to_string())
            .unwrap_or_else(|| "1".to_string());
        let returncode = item["returncode"].as_i64().unwrap_or(0);
        map.insert(
            fname.to_string(),
            RunIndexMeta {
                case_id: case_id.to_string(),
                arm: arm.to_string(),
                run_str,
                returncode,
            },
        );
    }
    map
}

fn split_case_and_arm(prefix: &str, by_case: &HashMap<String, &Value>) -> Option<(String, String)> {
    let mut best: Option<(&str, String)> = None;
    for id in by_case.keys() {
        let with_sep = format!("{id}_");
        if let Some(arm) = prefix.strip_prefix(&with_sep) {
            if !arm.is_empty()
                && best
                    .as_ref()
                    .is_none_or(|(best_id, _)| id.len() > best_id.len())
            {
                best = Some((id.as_str(), arm.to_string()));
            }
        }
    }
    if let Some((id, arm)) = best {
        return Some((id.to_string(), arm));
    }
    prefix
        .rsplit_once('_')
        .map(|(c, a)| (c.to_string(), a.to_string()))
}

#[derive(Default)]
struct ArmAgg {
    valid_runs: usize,
    failed_runs: usize,
    tp: usize,
    expected: usize,
    unmatched: usize,
    clean_fp: usize,
    missing_ctx: usize,
}

struct GradeSummary {
    rows: Vec<String>,
    agg: BTreeMap<String, ArmAgg>,
}

fn evaluate_grade_runs(rubrics: &Value, runs_dir: &Path) -> Result<GradeSummary, String> {
    let mut by_case: HashMap<String, &Value> = HashMap::new();
    if let Some(cases) = rubrics["cases"].as_array() {
        for c in cases {
            if let Some(id) = c["id"].as_str() {
                by_case.insert(id.to_string(), c);
            }
        }
    }

    let index_meta = load_run_index_meta(runs_dir);

    let mut files: Vec<PathBuf> = fs::read_dir(runs_dir)
        .map_err(|e| format!("read runs dir: {e}"))?
        .filter_map(|e| e.ok().map(|de| de.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    files.sort();

    let mut agg: BTreeMap<String, ArmAgg> = BTreeMap::new();
    let mut rows = Vec::new();

    for path in files {
        let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if fname == "llm_runs_index.json" {
            continue;
        }
        let (case_id, arm, run_str, returncode) = if let Some(meta) = index_meta.get(fname) {
            (
                meta.case_id.clone(),
                meta.arm.clone(),
                meta.run_str.clone(),
                meta.returncode,
            )
        } else {
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some((prefix, r_str)) = stem.rsplit_once("_run") else {
                continue;
            };
            let Some((cid, arm_name)) = split_case_and_arm(prefix, &by_case) else {
                continue;
            };
            (cid, arm_name, r_str.to_string(), 0)
        };
        let Some(rubric) = by_case.get(&case_id) else {
            continue;
        };

        let raw = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let parsed_opt = if returncode == 0 {
            extract_json_payload(&raw)
        } else {
            None
        };

        let Some(parsed) = parsed_opt else {
            let reason = if returncode != 0 {
                format!("ERR(rc={returncode})")
            } else {
                "INVALID_JSON".to_string()
            };
            let s = agg.entry(arm.clone()).or_default();
            s.failed_runs += 1;
            rows.push(format!(
                "| `{case_id}` | **{arm}** | {run_str} | {reason} | - | - |"
            ));
            continue;
        };

        let findings = parsed["findings"].as_array().cloned().unwrap_or_default();
        let missing = parsed["missing_context"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        let expected_bugs = rubric["expected_bugs"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut matched_bugs = 0usize;
        let mut used_idxs: HashSet<usize> = HashSet::new();

        for bug in &expected_bugs {
            let bug_file = bug["file"].as_str().unwrap_or("").to_ascii_lowercase();
            let bug_basename = Path::new(&bug_file)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&bug_file)
                .to_string();
            let keywords: Vec<String> = bug["keywords"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_ascii_lowercase()))
                        .collect()
                })
                .unwrap_or_default();
            for (idx, f) in findings.iter().enumerate() {
                let f_file = f["file"].as_str().unwrap_or("").to_ascii_lowercase();
                let content_blob = format!(
                    "{} {} {}",
                    f["title"].as_str().unwrap_or(""),
                    f["evidence"].as_str().unwrap_or(""),
                    f["explanation"].as_str().unwrap_or(""),
                )
                .to_ascii_lowercase();
                let file_hit = f_file.contains(&bug_file)
                    || f_file.contains(&bug_basename)
                    || content_blob.contains(&bug_file)
                    || content_blob.contains(&bug_basename);
                let kw_hits = keywords
                    .iter()
                    .filter(|kw| content_blob.contains(kw.as_str()))
                    .count();
                if file_hit && kw_hits >= 2 {
                    matched_bugs += 1;
                    used_idxs.insert(idx);
                    break;
                }
            }
        }

        let clean_files: Vec<String> = rubric["clean_files"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_ascii_lowercase()))
                    .collect()
            })
            .unwrap_or_default();
        let clean_fps = findings
            .iter()
            .filter(|f| {
                let fpath = f["file"].as_str().unwrap_or("").to_ascii_lowercase();
                clean_files.iter().any(|cf| fpath.contains(cf))
            })
            .count();

        let total_expected = expected_bugs.len();
        let extra = findings.len().saturating_sub(used_idxs.len());

        let s = agg.entry(arm.clone()).or_default();
        s.valid_runs += 1;
        s.tp += matched_bugs;
        s.expected += total_expected;
        s.unmatched += extra;
        s.clean_fp += clean_fps;
        s.missing_ctx += missing.len();

        rows.push(format!(
            "| `{case_id}` | **{arm}** | {run_str} | {matched_bugs}/{total_expected} | {extra} (clean FP: {clean_fps}) | {} |",
            missing.len()
        ));
    }

    Ok(GradeSummary { rows, agg })
}

fn cmd_grade(args: &[String]) -> Result<(), String> {
    let mut rubrics_path: Option<PathBuf> = None;
    let mut runs_dir: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--rubrics" => {
                rubrics_path = Some(PathBuf::from(next_flag_value(args, &mut i, "--rubrics")?))
            }
            "--runs-dir" => {
                runs_dir = Some(PathBuf::from(next_flag_value(args, &mut i, "--runs-dir")?))
            }
            other => return Err(format!("unknown flag for grade: {other}")),
        }
    }
    let rubrics_path = rubrics_path.ok_or("--rubrics is required")?;
    let runs_dir = runs_dir.ok_or("--runs-dir is required")?;

    let rubrics: Value = serde_json::from_str(
        &fs::read_to_string(&rubrics_path).map_err(|e| format!("read rubrics: {e}"))?,
    )
    .map_err(|e| format!("parse rubrics: {e}"))?;

    let summary = evaluate_grade_runs(&rubrics, &runs_dir)?;

    println!("| Case | Arm | Run | Bug Recall (TP) | Extra Findings | Missing Context Count |");
    println!("|---|---|---:|---:|---:|---:|");
    for r in summary.rows {
        println!("{r}");
    }
    println!("\n### Aggregate by Arm\n");
    println!(
        "| Arm | Runs (Valid/Failed) | Total Bug Recall | Extra Findings | Clean-File FPs | Missing Context Total |"
    );
    println!("|---|---:|---:|---:|---:|---:|");
    for (arm, s) in summary.agg {
        let pct = if s.expected > 0 {
            100.0 * (s.tp as f64) / (s.expected as f64)
        } else {
            0.0
        };
        println!(
            "| **{arm}** | {}/{} | {}/{} ({pct:.1}%) | {} | {} | {} |",
            s.valid_runs, s.failed_runs, s.tp, s.expected, s.unmatched, s.clean_fp, s.missing_ctx
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grade_handles_underscore_arm_and_failed_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let rubrics = json!({
            "cases": [{
                "id": "case_alpha_v1",
                "expected_bugs": [{
                    "file": "src/service.rs",
                    "keywords": ["overflow", "cents"]
                }],
                "clean_files": ["src/clean.rs"]
            }]
        });

        // 1. Underscore arm name (`l3_local`) WITHOUT relying on index filename parsing
        fs::write(
            dir.join("case_alpha_v1_l3_local_run1.json"),
            r#"{"findings":[{"file":"src/service.rs","line":10,"title":"Integer overflow","evidence":"cents","explanation":"cents overflow"}],"missing_context":[]}"#,
        )
        .unwrap();
        // 2. Non-JSON output -> should count as failed_runs, NOT 0/1 recall
        fs::write(
            dir.join("case_alpha_v1_B3_run1.json"),
            "error: anticrumb failed to connect",
        )
        .unwrap();
        // 2b. JSON error object without `findings` array -> must ALSO count as failed_runs
        fs::write(
            dir.join("case_alpha_v1_B1_run1.json"),
            r#"{"error": "rate_limited"}"#,
        )
        .unwrap();
        // 3. Valid run + failed returncode run tracked in llm_runs_index.json
        fs::write(
            dir.join("case_alpha_v1_A_run1.json"),
            r#"{"findings":[],"missing_context":[]}"#,
        )
        .unwrap();
        fs::write(
            dir.join("case_alpha_v1_A_run2.json"),
            r#"{"findings":[],"missing_context":[]}"#,
        )
        .unwrap();
        let index = json!([
            {
                "case_id": "case_alpha_v1",
                "arm": "A",
                "run": 1,
                "returncode": 0,
                "output_file": dir.join("case_alpha_v1_A_run1.json").display().to_string()
            },
            {
                "case_id": "case_alpha_v1",
                "arm": "A",
                "run": 2,
                "returncode": 1,
                "output_file": dir.join("case_alpha_v1_A_run2.json").display().to_string()
            }
        ]);
        fs::write(
            dir.join("llm_runs_index.json"),
            serde_json::to_string_pretty(&index).unwrap(),
        )
        .unwrap();

        let summary = evaluate_grade_runs(&rubrics, dir).unwrap();
        let l3 = &summary.agg["l3_local"];
        assert_eq!(l3.valid_runs, 1);
        assert_eq!(l3.failed_runs, 0);
        assert_eq!(l3.tp, 1);
        assert_eq!(l3.expected, 1);

        let b3 = &summary.agg["B3"];
        assert_eq!(b3.valid_runs, 0);
        assert_eq!(b3.failed_runs, 1);
        assert_eq!(b3.expected, 0);

        let b1 = &summary.agg["B1"];
        assert_eq!(b1.valid_runs, 0);
        assert_eq!(b1.failed_runs, 1);
        assert_eq!(b1.expected, 0);

        let a = &summary.agg["A"];
        assert_eq!(a.valid_runs, 1);
        assert_eq!(a.failed_runs, 1);
        assert_eq!(a.tp, 0);
        assert_eq!(a.expected, 1);
    }
}
