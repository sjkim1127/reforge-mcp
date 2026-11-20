//! Coverage-guided generation of verification inputs for an entry function.
//!
//! The original C sources are compiled with UBSan plus SanitizerCoverage
//! (`trace-pc-guard,trace-cmp`) and linked against a generated driver. The driver
//! forks one child per input, so undefined behavior, crashes, and hangs in the
//! source never take the generator down. Edge hit counts (AFL-style buckets) decide
//! which inputs join the corpus, comparison operands feed a value dictionary, and a
//! greedy set cover over the corpus picks the cases handed to `verify_port`. Up to 16
//! extra cases come from a uniform reservoir of clean runs, so straight-line code
//! still gets value-diverse inputs.
use anyhow::{Context, Result, bail, ensure};

use crate::harness::{EntrySignature, abi_shims, c_prototype, is_int_pointer};
use crate::model::{FixtureValue, GeneratedFailure};
use crate::{is_supported_entry_float, is_supported_entry_int};

/// UBSan exit code used by the driver to tell undefined behavior from other failures.
const UB_EXIT_CODE: i32 = 86;

pub(crate) const COVERAGE_FLAGS: &str = "-fsanitize-coverage=trace-pc-guard,trace-cmp";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Signed,
    Unsigned,
    Bool,
    F32,
    F64,
    IntPointer,
}

impl Kind {
    fn code(self) -> u8 {
        match self {
            Self::Signed => 0,
            Self::Unsigned => 1,
            Self::Bool => 2,
            Self::F32 => 3,
            Self::F64 => 4,
            Self::IntPointer => 5,
        }
    }
}

fn param_kind(ty: &str) -> Result<(Kind, u32)> {
    if is_int_pointer(ty) {
        return Ok((Kind::IntPointer, 32));
    }
    if let Some(fty) = is_supported_entry_float(ty) {
        return Ok(if fty == "f32" {
            (Kind::F32, 32)
        } else {
            (Kind::F64, 64)
        });
    }
    if ty == "_Bool" {
        return Ok((Kind::Bool, 8));
    }
    let (rty, unsigned) =
        is_supported_entry_int(ty).with_context(|| format!("unsupported parameter type {ty:?}"))?;
    let bits = rty[1..]
        .parse::<u32>()
        .with_context(|| format!("unexpected Rust integer type {rty}"))?;
    Ok((
        if unsigned {
            Kind::Unsigned
        } else {
            Kind::Signed
        },
        bits,
    ))
}

/// Render the fork-based fuzzing driver for `entry`.
pub(crate) fn render_driver(
    job: &crate::model::Job,
    entry: &str,
    sig: &EntrySignature,
) -> Result<String> {
    let kinds = sig
        .param_types
        .iter()
        .map(|ty| param_kind(ty))
        .collect::<Result<Vec<_>>>()?;
    let table = |f: &dyn Fn(&(Kind, u32)) -> String| {
        if kinds.is_empty() {
            "0".to_string()
        } else {
            kinds.iter().map(f).collect::<Vec<_>>().join(", ")
        }
    };
    let kind_table = table(&|(k, _)| k.code().to_string());
    let bits_table = table(&|(_, b)| b.to_string());

    let mut pre_call = String::new();
    let mut args = Vec::with_capacity(kinds.len());
    for (i, ((kind, _), ty)) in kinds.iter().zip(&sig.param_types).enumerate() {
        match kind {
            Kind::IntPointer => {
                pre_call.push_str(&format!("    int rf_p{i} = (int)(int64_t)in->a[{i}].u;\n"));
                args.push(format!("&rf_p{i}"));
            }
            Kind::Signed => args.push(format!("({ty})(int64_t)in->a[{i}].u")),
            Kind::Unsigned => args.push(format!("({ty})in->a[{i}].u")),
            Kind::Bool => args.push(format!("(_Bool)(in->a[{i}].u != 0)")),
            Kind::F32 | Kind::F64 => args.push(format!("({ty})in->a[{i}].d")),
        }
    }
    // Keep the result observable: libc headers may declare a same-named function
    // (`abs`) as pure, which would otherwise let an optimizer drop the call.
    let call = if sig.return_type == "void" {
        format!("{pre_call}    {entry}({});\n", args.join(", "))
    } else {
        format!(
            "{pre_call}    volatile {} rf_sink = {entry}({});\n    (void)rf_sink;\n",
            sig.return_type,
            args.join(", ")
        )
    };

    Ok(DRIVER_TEMPLATE
        .replace(
            "@PRELUDE@",
            &format!(
                "{}{}",
                abi_shims(job, entry),
                c_prototype(entry, &sig.param_types, &sig.return_type)
            ),
        )
        .replace("@NPARAMS@", &kinds.len().to_string())
        .replace("@KINDS@", &kind_table)
        .replace("@BITS@", &bits_table)
        .replace("@CALL@", &call)
        .replace("@UB_EXIT@", &UB_EXIT_CODE.to_string()))
}

/// Parsed driver output.
#[derive(Debug, Default)]
pub(crate) struct DriverOutput {
    pub cases: Vec<Vec<FixtureValue>>,
    pub undefined_behavior: Vec<Vec<FixtureValue>>,
    pub source_failures: Vec<GeneratedFailure>,
    pub timeouts: Vec<Vec<FixtureValue>>,
    pub stats: std::collections::BTreeMap<String, u64>,
}

pub(crate) fn parse_output(stdout: &str, sig: &EntrySignature) -> Result<DriverOutput> {
    let kinds = sig
        .param_types
        .iter()
        .map(|ty| param_kind(ty).map(|(k, _)| k))
        .collect::<Result<Vec<_>>>()?;
    let values = |tokens: &[&str]| -> Result<Vec<FixtureValue>> {
        ensure!(
            tokens.len() == kinds.len(),
            "case generator emitted {} values for {} parameters",
            tokens.len(),
            kinds.len()
        );
        tokens
            .iter()
            .zip(&kinds)
            .map(|(token, kind)| {
                Ok(match kind {
                    Kind::Signed | Kind::IntPointer => FixtureValue::Int(token.parse()?),
                    Kind::Unsigned | Kind::Bool => FixtureValue::from_u64(token.parse()?),
                    Kind::F32 | Kind::F64 => {
                        let v: f64 = token.parse()?;
                        ensure!(v.is_finite(), "case generator emitted non-finite {token}");
                        FixtureValue::Float(v)
                    }
                })
            })
            .collect()
    };
    let mut out = DriverOutput::default();
    let mut done = false;
    for line in stdout.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let Some((tag, rest)) = tokens.split_first() else {
            continue;
        };
        match *tag {
            "RF_CASE" => out.cases.push(values(rest)?),
            "RF_UB" => out.undefined_behavior.push(values(rest)?),
            "RF_TIMEOUT" => out.timeouts.push(values(rest)?),
            "RF_FAIL" => {
                let (outcome, rest) = rest.split_first().context("RF_FAIL without outcome")?;
                out.source_failures.push(GeneratedFailure {
                    arguments: values(rest)?,
                    outcome: (*outcome).to_string(),
                });
            }
            "RF_STATS" => {
                for pair in rest {
                    let (key, value) = pair.split_once('=').context("malformed RF_STATS")?;
                    out.stats.insert(key.to_string(), value.parse()?);
                }
            }
            "RF_DONE" => done = true,
            _ => bail!("unexpected case generator output: {line}"),
        }
    }
    ensure!(done, "case generator output is incomplete");
    Ok(out)
}

const DRIVER_TEMPLATE: &str = r#"/* Generated by reforge-mcp: coverage-guided verification input generator. */
#define _DEFAULT_SOURCE
#define _DARWIN_C_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <float.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <unistd.h>

@PRELUDE@
#define RF_SINT 0
#define RF_UINT 1
#define RF_BOOL 2
#define RF_F32 3
#define RF_F64 4
#define RF_IPTR 5
#define RF_N @NPARAMS@
#define RF_NA (RF_N > 0 ? RF_N : 1)
#define RF_UB_EXIT @UB_EXIT@
#define RF_EXEC_TIMEOUT_S 1
#define RF_CMP_CAP 512
#define RF_DICT_CAP 1024
#define RF_DICT_SLOTS 4096
#define RF_CORPUS_CAP 2048
#define RF_SAMPLE_CAP 16
#define RF_RESERVOIR 64
#define RF_DIVERSITY_MAX 16

static const int rf_kind[RF_NA] = {@KINDS@};
static const int rf_bits[RF_NA] = {@BITS@};

typedef union { uint64_t u; double d; } rf_val;
typedef struct { rf_val a[RF_NA]; } rf_input;

static void rf_call(const rf_input *in) {
    (void)in;
@CALL@}

const char *__ubsan_default_options(void) {
    return "exitcode=86:abort_on_error=0:halt_on_error=1:print_stacktrace=0:print_summary=0";
}

/* ---- SanitizerCoverage callbacks (this file is compiled without coverage) ---- */
static uint32_t rf_nedges;
static uint8_t *rf_counters;
typedef struct { uint32_t n; uint64_t v[RF_CMP_CAP]; } rf_cmplog;
static rf_cmplog *rf_cmp;

void __sanitizer_cov_trace_pc_guard_init(uint32_t *start, uint32_t *stop) {
    if (start == stop || *start) return;
    for (uint32_t *g = start; g < stop; g++) *g = ++rf_nedges;
}
void __sanitizer_cov_trace_pc_guard(uint32_t *guard) {
    if (rf_counters && *guard) {
        uint8_t c = rf_counters[*guard];
        if (c != 255) rf_counters[*guard] = (uint8_t)(c + 1);
    }
}
static void rf_log(uint64_t v, unsigned width) {
    if (!rf_cmp) return;
    if (rf_cmp->n < RF_CMP_CAP) rf_cmp->v[rf_cmp->n++] = v;
    if (width > 0 && width < 64 && ((v >> (width - 1)) & 1) && rf_cmp->n < RF_CMP_CAP)
        rf_cmp->v[rf_cmp->n++] = v | (~(uint64_t)0 << width);
}
void __sanitizer_cov_trace_cmp1(uint8_t a, uint8_t b) { rf_log(a, 8); rf_log(b, 8); }
void __sanitizer_cov_trace_cmp2(uint16_t a, uint16_t b) { rf_log(a, 16); rf_log(b, 16); }
void __sanitizer_cov_trace_cmp4(uint32_t a, uint32_t b) { rf_log(a, 32); rf_log(b, 32); }
void __sanitizer_cov_trace_cmp8(uint64_t a, uint64_t b) { rf_log(a, 64); rf_log(b, 64); }
void __sanitizer_cov_trace_const_cmp1(uint8_t a, uint8_t b) { (void)b; rf_log(a, 8); }
void __sanitizer_cov_trace_const_cmp2(uint16_t a, uint16_t b) { (void)b; rf_log(a, 16); }
void __sanitizer_cov_trace_const_cmp4(uint32_t a, uint32_t b) { (void)b; rf_log(a, 32); }
void __sanitizer_cov_trace_const_cmp8(uint64_t a, uint64_t b) { (void)b; rf_log(a, 64); }
void __sanitizer_cov_trace_switch(uint64_t val, uint64_t *cases) {
    (void)val;
    unsigned width = cases[1] ? (unsigned)cases[1] : 64;
    for (uint64_t i = 0; i < cases[0]; i++) rf_log(cases[2 + i], width);
}

/* ---- Random values ---- */
static uint64_t rf_state;
static uint64_t rf_rand(void) {
    rf_state ^= rf_state << 13;
    rf_state ^= rf_state >> 7;
    rf_state ^= rf_state << 17;
    return rf_state;
}
static uint64_t rf_below(uint64_t n) { return n ? rf_rand() % n : 0; }

static const int64_t rf_int_seeds[] = {
    0, 1, 2, 3, 4, 7, 8, 10, 15, 16, 31, 32, 63, 64, 100, 127, 128, 255, 256, 511, 512,
    1000, 1023, 1024, 4095, 4096, 32767, 32768, 65535, 65536, 2147483647LL, 2147483648LL,
    4294967295LL, 4294967296LL, INT64_MAX};
#define RF_INT_SEEDS (sizeof rf_int_seeds / sizeof rf_int_seeds[0])
static const double rf_float_seeds[] = {
    0.0, 0.5, 1.0, 2.0, 3.0, 10.0, 100.0, 0.1, 0.25, 1e-6, 1e6, 3.141592653589793,
    2.718281828459045, 1e-300, 1e300, DBL_MIN, DBL_MAX, FLT_MIN, FLT_MAX, DBL_EPSILON,
    2147483648.0, 9007199254740992.0, 1e15};
#define RF_FLOAT_SEEDS (sizeof rf_float_seeds / sizeof rf_float_seeds[0])

static int rf_is_float(int i) { return rf_kind[i] == RF_F32 || rf_kind[i] == RF_F64; }

static uint64_t rf_wrap(int i, uint64_t v) {
    int bits = rf_bits[i];
    switch (rf_kind[i]) {
    case RF_BOOL:
        return v != 0;
    case RF_UINT:
        return bits >= 64 ? v : v & ((UINT64_C(1) << bits) - 1);
    default: {
        if (bits >= 64) return v;
        uint64_t mask = (UINT64_C(1) << bits) - 1;
        v &= mask;
        if ((v >> (bits - 1)) & 1) v |= ~mask;
        return v;
    }
    }
}
static double rf_fnorm(int i, double d) {
    if (!(d == d) || d > DBL_MAX || d < -DBL_MAX) return 0.0;
    if (rf_kind[i] == RF_F32) {
        if (d > FLT_MAX) d = FLT_MAX;
        if (d < -FLT_MAX) d = -FLT_MAX;
        d = (double)(float)d;
    }
    return d;
}
static uint64_t rf_d2u(double d) {
    return (d > -9.2e18 && d < 9.2e18) ? (uint64_t)(int64_t)d : 0;
}
static uint64_t rf_interesting(int i) {
    int bits = rf_bits[i];
    if (rf_below(4) == 0) {
        uint64_t max, min;
        if (rf_kind[i] == RF_UINT) {
            max = bits >= 64 ? UINT64_MAX : (UINT64_C(1) << bits) - 1;
            min = 0;
        } else {
            max = bits >= 64 ? (uint64_t)INT64_MAX : (UINT64_C(1) << (bits - 1)) - 1;
            min = (uint64_t)0 - max - 1;
        }
        switch (rf_below(4)) {
        case 0: return max;
        case 1: return max - 1;
        case 2: return min;
        default: return min + 1;
        }
    }
    uint64_t v = (uint64_t)rf_int_seeds[rf_below(RF_INT_SEEDS)];
    return rf_below(2) ? (uint64_t)0 - v : v;
}

/* ---- Comparison-operand dictionary ---- */
static uint64_t rf_dict[RF_DICT_CAP];
static size_t rf_dict_n;
static uint64_t rf_dict_slot[RF_DICT_SLOTS];
static uint8_t rf_dict_used[RF_DICT_SLOTS];
static void rf_dict_add(uint64_t v) {
    if (rf_dict_n >= RF_DICT_CAP) return;
    size_t h = (size_t)((v * UINT64_C(0x9E3779B97F4A7C15)) >> 52) & (RF_DICT_SLOTS - 1);
    while (rf_dict_used[h]) {
        if (rf_dict_slot[h] == v) return;
        h = (h + 1) & (RF_DICT_SLOTS - 1);
    }
    rf_dict_used[h] = 1;
    rf_dict_slot[h] = v;
    rf_dict[rf_dict_n++] = v;
}

static void rf_mutate_one(rf_input *in, int i) {
    if (rf_kind[i] == RF_BOOL) {
        in->a[i].u ^= 1;
        return;
    }
    if (rf_is_float(i)) {
        double d = in->a[i].d;
        switch (rf_below(8)) {
        case 0: d = rf_float_seeds[rf_below(RF_FLOAT_SEEDS)] * (rf_below(2) ? -1.0 : 1.0); break;
        case 1: d += (double)((int64_t)rf_below(21) - 10); break;
        case 2: d *= rf_below(2) ? 2.0 : 0.5; break;
        case 3: d = -d; break;
        case 4:
            if (rf_dict_n) d = (double)(int64_t)rf_dict[rf_below(rf_dict_n)] + 0.5 * ((double)rf_below(3) - 1.0);
            break;
        case 5: d = (double)(int64_t)rf_rand() / (double)(UINT64_C(1) << rf_below(63)); break;
        case 6:
            if (RF_N > 1) {
                int j = (int)rf_below(RF_N);
                d = rf_is_float(j) ? in->a[j].d : (double)(int64_t)in->a[j].u;
            }
            break;
        default: d += d == 0.0 ? DBL_MIN : d * DBL_EPSILON * (double)(1 + rf_below(4)); break;
        }
        in->a[i].d = rf_fnorm(i, d);
        return;
    }
    uint64_t v = in->a[i].u;
    switch (rf_below(9)) {
    case 0: v = rf_interesting(i); break;
    case 1: v += (uint64_t)((int64_t)rf_below(71) - 35); break;
    case 2: v ^= UINT64_C(1) << rf_below((uint64_t)rf_bits[i]); break;
    case 3:
        v = rf_dict_n ? rf_dict[rf_below(rf_dict_n)] + (uint64_t)((int64_t)rf_below(3) - 1)
                      : rf_interesting(i);
        break;
    case 4: v = rf_rand(); break;
    case 5: v = (uint64_t)0 - v; break;
    case 6:
        if (RF_N > 1) {
            int j = (int)rf_below(RF_N);
            v = rf_is_float(j) ? rf_d2u(in->a[j].d) : in->a[j].u;
        }
        break;
    case 7: v = rf_below(2) ? v << 1 : (uint64_t)((int64_t)v >> 1); break;
    default: v = (uint64_t)((int64_t)rf_below(33) - 16); break;
    }
    in->a[i].u = rf_wrap(i, v);
}

/* ---- Execution ---- */
enum { RF_OK, RF_UB, RF_TIMEOUT, RF_FAIL };
static int rf_devnull = -1;
static int rf_last_status;

static int rf_exec(const rf_input *in) {
    memset(rf_counters, 0, (size_t)rf_nedges + 1);
    rf_cmp->n = 0;
    fflush(stdout);
    pid_t pid = fork();
    if (pid < 0) {
        perror("fork");
        exit(3);
    }
    if (pid == 0) {
        if (rf_devnull >= 0) {
            dup2(rf_devnull, 1);
            dup2(rf_devnull, 2);
        }
        alarm(RF_EXEC_TIMEOUT_S);
        rf_call(in);
        _exit(0);
    }
    int status = 0;
    while (waitpid(pid, &status, 0) < 0) {
        if (errno != EINTR) {
            perror("waitpid");
            exit(3);
        }
    }
    rf_last_status = status;
    if (WIFEXITED(status)) {
        int code = WEXITSTATUS(status);
        if (code == 0) return RF_OK;
        return code == RF_UB_EXIT ? RF_UB : RF_FAIL;
    }
    if (WIFSIGNALED(status) && WTERMSIG(status) == SIGALRM) return RF_TIMEOUT;
    return RF_FAIL;
}

/* ---- Corpus ---- */
typedef struct { rf_input in; uint32_t *feat; uint32_t nfeat; } rf_entry;
static rf_entry rf_corpus[RF_CORPUS_CAP];
static size_t rf_ncorpus;
static uint8_t *rf_seen;
static uint32_t *rf_tmp;
static unsigned long long rf_execs, rf_features, rf_n_ub, rf_n_fail, rf_n_timeout;

typedef struct { rf_input in; char outcome[24]; } rf_sample;
static rf_sample rf_ub[RF_SAMPLE_CAP], rf_fail[RF_SAMPLE_CAP], rf_timeout[RF_SAMPLE_CAP];
static size_t rf_ub_n, rf_fail_n, rf_timeout_n;
/* Uniform reservoir of clean runs: value diversity for code with little branching. */
static rf_input rf_reservoir[RF_RESERVOIR];
static size_t rf_reservoir_n;
static unsigned long long rf_ok_runs;

static int rf_bucket(uint8_t c) {
    if (c <= 3) return c - 1;
    if (c < 8) return 3;
    if (c < 16) return 4;
    if (c < 32) return 5;
    if (c < 128) return 6;
    return 7;
}

static void rf_keep(rf_sample *samples, size_t *n, const rf_input *in) {
    if (*n >= RF_SAMPLE_CAP) return;
    for (size_t k = 0; k < *n; k++)
        if (memcmp(&samples[k].in, in, sizeof *in) == 0) return;
    samples[*n].in = *in;
    if (WIFEXITED(rf_last_status))
        snprintf(samples[*n].outcome, sizeof samples[*n].outcome, "exit:%d", WEXITSTATUS(rf_last_status));
    else
        snprintf(samples[*n].outcome, sizeof samples[*n].outcome, "signal:%d", WTERMSIG(rf_last_status));
    (*n)++;
}

static void rf_run(const rf_input *in) {
    rf_execs++;
    int outcome = rf_exec(in);
    for (uint32_t k = 0; k < rf_cmp->n; k++) rf_dict_add(rf_cmp->v[k]);
    if (outcome == RF_UB) { rf_n_ub++; rf_keep(rf_ub, &rf_ub_n, in); return; }
    if (outcome == RF_TIMEOUT) { rf_n_timeout++; rf_keep(rf_timeout, &rf_timeout_n, in); return; }
    if (outcome == RF_FAIL) { rf_n_fail++; rf_keep(rf_fail, &rf_fail_n, in); return; }
    rf_ok_runs++;
    if (rf_reservoir_n < RF_RESERVOIR) {
        rf_reservoir[rf_reservoir_n++] = *in;
    } else {
        uint64_t slot = rf_below(rf_ok_runs);
        if (slot < RF_RESERVOIR) rf_reservoir[slot] = *in;
    }
    uint32_t nfeat = 0;
    int novel = 0;
    for (uint32_t e = 1; e <= rf_nedges; e++) {
        uint8_t c = rf_counters[e];
        if (!c) continue;
        int b = rf_bucket(c);
        rf_tmp[nfeat++] = (e << 3) | (uint32_t)b;
        if (!(rf_seen[e] & (1u << b))) {
            rf_seen[e] |= (uint8_t)(1u << b);
            rf_features++;
            novel = 1;
        }
    }
    if (!novel || rf_ncorpus >= RF_CORPUS_CAP) return;
    rf_entry *entry = &rf_corpus[rf_ncorpus];
    entry->feat = malloc(sizeof(uint32_t) * (nfeat ? nfeat : 1));
    if (!entry->feat) return;
    memcpy(entry->feat, rf_tmp, sizeof(uint32_t) * nfeat);
    entry->nfeat = nfeat;
    entry->in = *in;
    rf_ncorpus++;
}

static long long rf_now_ms(void) {
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return (long long)tv.tv_sec * 1000 + tv.tv_usec / 1000;
}

static void rf_print_values(const rf_input *in) {
    for (int i = 0; i < RF_N; i++) {
        switch (rf_kind[i]) {
        case RF_SINT:
        case RF_IPTR: printf(" %lld", (long long)(int64_t)in->a[i].u); break;
        case RF_UINT:
        case RF_BOOL: printf(" %llu", (unsigned long long)in->a[i].u); break;
        default: printf(" %.17g", in->a[i].d); break;
        }
    }
    printf("\n");
}

int main(int argc, char **argv) {
    if (argc != 5) return 2;
    uint64_t seed = strtoull(argv[1], 0, 10);
    unsigned long long max_execs = strtoull(argv[2], 0, 10);
    long long budget_ms = strtoll(argv[3], 0, 10);
    int max_cases = atoi(argv[4]);
    rf_state = seed ^ UINT64_C(0x9E3779B97F4A7C15);
    if (!rf_state) rf_state = 1;

    rf_counters = mmap(NULL, (size_t)rf_nedges + 1, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANON, -1, 0);
    rf_cmp = mmap(NULL, sizeof(rf_cmplog), PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANON, -1, 0);
    rf_seen = calloc((size_t)rf_nedges + 1, 1);
    rf_tmp = malloc(sizeof(uint32_t) * ((size_t)rf_nedges + 1));
    if (rf_counters == MAP_FAILED || rf_cmp == MAP_FAILED || !rf_seen || !rf_tmp) {
        perror("allocation");
        return 3;
    }
    rf_devnull = open("/dev/null", O_WRONLY);
    long long start = rf_now_ms();
#define RF_BUDGET_LEFT() (rf_execs < max_execs && rf_now_ms() - start < budget_ms)

    rf_input zero;
    memset(&zero, 0, sizeof zero);
    rf_run(&zero);
    for (int i = 0; i < RF_N && RF_BUDGET_LEFT(); i++) {
        if (rf_is_float(i)) {
            for (size_t k = 0; k < RF_FLOAT_SEEDS * 2 && RF_BUDGET_LEFT(); k++) {
                rf_input in = zero;
                in.a[i].d = rf_fnorm(i, rf_float_seeds[k / 2] * (k % 2 ? -1.0 : 1.0));
                rf_run(&in);
            }
        } else {
            for (size_t k = 0; k < RF_INT_SEEDS * 2 && RF_BUDGET_LEFT(); k++) {
                rf_input in = zero;
                uint64_t v = (uint64_t)rf_int_seeds[k / 2];
                in.a[i].u = rf_wrap(i, k % 2 ? (uint64_t)0 - v : v);
                rf_run(&in);
            }
        }
    }
    while (RF_N > 0 && RF_BUDGET_LEFT()) {
        rf_input child = rf_ncorpus ? rf_corpus[rf_below(rf_ncorpus)].in : zero;
        int rounds = rf_below(4) == 0 ? 2 + (int)rf_below(3) : 1;
        for (int r = 0; r < rounds; r++) rf_mutate_one(&child, (int)rf_below(RF_N));
        rf_run(&child);
    }

    /* Greedy set cover over (edge, hit-count bucket) features. */
    uint8_t *covered = calloc((size_t)rf_nedges + 1, 1);
    uint8_t *picked = calloc(rf_ncorpus ? rf_ncorpus : 1, 1);
    if (!covered || !picked) {
        perror("allocation");
        return 3;
    }
    unsigned long long selected_features = 0, coverage_cases = 0, diversity_cases = 0;
    rf_input *emitted = malloc(sizeof(rf_input) * ((size_t)max_cases + 1));
    if (!emitted) {
        perror("allocation");
        return 3;
    }
    for (int k = 0; k < max_cases; k++) {
        size_t best = (size_t)-1;
        uint32_t best_gain = 0;
        for (size_t c = 0; c < rf_ncorpus; c++) {
            if (picked[c]) continue;
            uint32_t gain = 0;
            for (uint32_t f = 0; f < rf_corpus[c].nfeat; f++) {
                uint32_t e = rf_corpus[c].feat[f] >> 3, b = 1u << (rf_corpus[c].feat[f] & 7);
                if (!(covered[e] & b)) gain++;
            }
            if (gain > best_gain) {
                best_gain = gain;
                best = c;
            }
        }
        if (best == (size_t)-1) break;
        picked[best] = 1;
        for (uint32_t f = 0; f < rf_corpus[best].nfeat; f++) {
            uint32_t e = rf_corpus[best].feat[f] >> 3, b = 1u << (rf_corpus[best].feat[f] & 7);
            covered[e] |= (uint8_t)b;
        }
        selected_features += best_gain;
        emitted[coverage_cases++] = rf_corpus[best].in;
        printf("RF_CASE");
        rf_print_values(&rf_corpus[best].in);
    }
    for (size_t r = 0; r < rf_reservoir_n; r++) {
        if (coverage_cases + diversity_cases >= (unsigned long long)max_cases ||
            diversity_cases >= RF_DIVERSITY_MAX)
            break;
        int duplicate = 0;
        for (unsigned long long k = 0; k < coverage_cases + diversity_cases && !duplicate; k++)
            duplicate = memcmp(&emitted[k], &rf_reservoir[r], sizeof(rf_input)) == 0;
        if (duplicate) continue;
        emitted[coverage_cases + diversity_cases++] = rf_reservoir[r];
        printf("RF_CASE");
        rf_print_values(&rf_reservoir[r]);
    }
    for (size_t k = 0; k < rf_ub_n; k++) { printf("RF_UB"); rf_print_values(&rf_ub[k].in); }
    for (size_t k = 0; k < rf_timeout_n; k++) { printf("RF_TIMEOUT"); rf_print_values(&rf_timeout[k].in); }
    for (size_t k = 0; k < rf_fail_n; k++) { printf("RF_FAIL %s", rf_fail[k].outcome); rf_print_values(&rf_fail[k].in); }
    unsigned long long covered_edges = 0;
    for (uint32_t e = 1; e <= rf_nedges; e++) covered_edges += rf_seen[e] != 0;
    printf("RF_STATS executions=%llu corpus=%llu instrumented_edges=%llu covered_edges=%llu features=%llu selected_features=%llu coverage_cases=%llu diversity_cases=%llu undefined_behavior=%llu source_failures=%llu timeouts=%llu elapsed_ms=%lld\n",
           rf_execs, (unsigned long long)rf_ncorpus, (unsigned long long)rf_nedges, covered_edges,
           rf_features, selected_features, coverage_cases, diversity_cases, rf_n_ub, rf_n_fail,
           rf_n_timeout, rf_now_ms() - start);
    printf("RF_DONE\n");
    return 0;
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn signature(types: &[&str]) -> EntrySignature {
        EntrySignature {
            param_types: types.iter().map(|t| t.to_string()).collect(),
            return_type: "int".into(),
        }
    }

    #[test]
    fn maps_parameter_types_to_kinds_and_widths() {
        assert_eq!(param_kind("int").unwrap(), (Kind::Signed, 32));
        assert_eq!(param_kind("unsigned char").unwrap(), (Kind::Unsigned, 8));
        assert_eq!(
            param_kind("unsigned long long").unwrap(),
            (Kind::Unsigned, 64)
        );
        assert_eq!(param_kind("_Bool").unwrap(), (Kind::Bool, 8));
        assert_eq!(param_kind("float").unwrap(), (Kind::F32, 32));
        assert_eq!(param_kind("const int *").unwrap(), (Kind::IntPointer, 32));
        assert!(param_kind("struct point").is_err());
    }

    #[test]
    fn parses_typed_driver_output() {
        let sig = signature(&["int", "unsigned long long", "double"]);
        let out = parse_output(
            "RF_CASE -1 18446744073709551615 0.10000000000000001\n\
             RF_UB 2147483647 0 0\n\
             RF_FAIL signal:11 1 2 -0\n\
             RF_STATS executions=10 covered_edges=3\n\
             RF_DONE\n",
            &sig,
        )
        .unwrap();
        assert_eq!(
            out.cases,
            vec![vec![
                FixtureValue::Int(-1),
                FixtureValue::UInt(u64::MAX),
                FixtureValue::Float(0.1),
            ]]
        );
        assert_eq!(out.undefined_behavior[0][0], FixtureValue::Int(2147483647));
        assert_eq!(out.source_failures[0].outcome, "signal:11");
        assert_eq!(out.stats["executions"], 10);
    }

    #[test]
    fn rejects_truncated_or_malformed_output() {
        let sig = signature(&["int"]);
        assert!(parse_output("RF_CASE 1\n", &sig).is_err());
        assert!(parse_output("RF_CASE 1 2\nRF_DONE\n", &sig).is_err());
        assert!(parse_output("RF_CASE 1.5\nRF_DONE\n", &sig).is_err());
        assert!(parse_output("garbage\nRF_DONE\n", &sig).is_err());
    }
}
