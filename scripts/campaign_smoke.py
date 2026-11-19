#!/usr/bin/env python3
"""Actual MCP campaign: strtol dependency mismatch, adapter, revision and restart."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
from porting_smoke import Client, ROOT


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/debug/reforge-mcp')
    parser.add_argument('--data-dir', type=Path, default=ROOT / '.reforge-demo')
    args = parser.parse_args()
    data = args.data_dir.resolve()
    data.mkdir(parents=True, exist_ok=True)
    work = Path(tempfile.mkdtemp(prefix='campaign-', dir=data))
    source, rust, raw = [work / name for name in ('source', 'replacement', 'raw')]
    for path in (source, rust, raw):
        path.mkdir()
        (path / 'target').mkdir()
    shutil.copy(ROOT / 'fixtures/migration/strtol-source/probe.c', source / 'probe.c')
    shutil.copy(ROOT / 'fixtures/migration/strtol-rust/src/main.rs', rust / 'main.rs')
    (source / 'pom.xml').write_text('<project><dependencies/></project>\n')
    (source / 'App.java').write_text('class App {}\n')
    (raw / 'main.rs').write_text('fn main(){let x=std::env::args().nth(1).unwrap();match x.parse::<i32>(){Ok(v)=>println!("{{\\\"value\\\":{v}}}"),Err(_)=>println!("{{\\\"error\\\":\\\"invalid\\\"}}")}}\n')
    subprocess.run(['clang', str(source / 'probe.c'), '-o', str(source / 'probe')], check=True)
    for path in (rust, raw):
        subprocess.run(['rustc', '--edition=2024', str(path / 'main.rs'), '-o', str(path / 'target/probe')], check=True)
    version = subprocess.check_output(['rustc', '--version'], text=True).strip()
    requirement = {'ecosystem': 'c', 'package': 'libc strtol caller', 'version': 'C11 API; ambient runtime',
                   'used_apis': ['strtol'], 'usage_refs': [{'file': 'probe.c', 'line_start': 8, 'line_end': 11}],
                   'unknown_usage': [], 'contracts': [{'id': 'decimal_int32', 'description': 'whole decimal int32; ASCII leading whitespace/sign; invalid and overflow errors', 'mandatory': True}]}
    def candidate(path):
        return {'package': 'Rust std plus lexical adapter', 'version': version, 'strategy': 'adapter',
                'source_path': str(path), 'research_urls': ['https://doc.rust-lang.org/std/primitive.i32.html#method.from_str'],
                'license': 'MIT OR Apache-2.0 (agent assertion)', 'target': 'host native',
                'api_mapping': {'strtol': 'str::parse::<i32> + lexical adapter'}, 'known_gaps': [],
                'adapter_tasks': ['preserve whitespace/error precedence'], 'dependency_campaign_id': None}
    cases = [[x] for x in ['42', ' +42', '\v-7', '12x', '2147483648', '', '-2147483648', '999999999999999999999x']]
    def probes(path):
        return [{'contract_id': 'decimal_int32', 'source': {'program': str(source / 'probe'), 'args': []},
                 'replacement': {'program': str(path / 'target/probe'), 'args': []}, 'cases': cases}]
    binary = args.binary.resolve()
    client = Client(binary, data)
    try:
        campaign = client.tool('inventory_project', {'name': 'stdlib replacement MCP demo', 'source_path': str(source)})
        cid = campaign['campaign_id']
        assert campaign['languages']['java'] == 1 and 'pom.xml' in campaign['manifests']
        page = client.tool('get_migration_inventory', {'campaign_id': cid, 'limit': 1})
        assert page['next_offset'] == 1
        context = client.tool('get_migration_source_context', {'campaign_id': cid, 'file_path': 'probe.c', 'limit': 4096})
        assert 'strtol' in context['text']
        req = client.tool('register_dependency_requirements', {'campaign_id': cid, 'requirement': requirement})
        rid = req['requirement_id']
        tasks = client.tool('next_migration_tasks', {'campaign_id': cid})
        assert tasks['tasks'][0]['action'] == 'search_rust_candidates_or_port_dependency'
        wrong = client.tool('record_dependency_candidate', {'requirement_id': rid, 'candidate': candidate(raw)})
        mismatch = client.tool('verify_dependency_replacement', {'candidate_id': wrong['candidate_id'], 'probes': probes(raw)})
        assert mismatch['status'] == 'mismatch'
        fixed = client.tool('record_dependency_candidate', {'requirement_id': rid, 'candidate': candidate(rust)})
        fid = fixed['candidate_id']
        plan = client.tool('plan_dependency_adapter', {'candidate_id': fid})
        assert plan['acceptance_granted'] is False
        evidence = client.tool('verify_dependency_replacement', {'candidate_id': fid, 'probes': probes(rust)})
        assert evidence['status'] == 'passed' and len(evidence['cases']) == 8
        requirement['contracts'][0]['description'] += '; revised audit wording'
        revised = client.tool('revise_dependency_requirements', {'requirement_id': rid, 'requirement': requirement})
        assert revised['revision'] == 2
        view = client.tool('get_dependency_requirements', {'campaign_id': cid})
        assert view['requirements'][0]['state'] == 'unresolved'
        evidence = client.tool('verify_dependency_replacement', {'candidate_id': fid, 'probes': probes(rust)})
        assert evidence['status'] == 'passed'
    finally:
        client.close()
    resumed = Client(binary, data)
    try:
        view = resumed.tool('get_dependency_requirements', {'campaign_id': cid})
        assert view['requirements'][0]['state'] == 'accepted_fixture_scope'
        report = resumed.tool('get_migration_campaign', {'campaign_id': cid})
        assert report['program_migration_completed'] is False
        tasks = resumed.tool('next_migration_tasks', {'campaign_id': cid})
        assert tasks['tasks'][0]['action'] == 'continue_program_conversion_and_integration'
        output = data / 'campaign-report.json'
        output.write_text(json.dumps({'campaign': report, 'requirements': view, 'adapter_plan': plan,
                                      'tasks': tasks, 'mismatch': mismatch, 'restart_resume': True,
                                      'provenance': 'Client-authored probes/adapter; compiled before inventory. Not an automatic dependency port.'}, indent=2) + '\n')
        print(json.dumps({'campaign_id': cid, 'matched_cases': 8, 'raw_candidate': 'mismatch',
                          'adapter': 'accepted_fixture_scope', 'revision_invalidation': True,
                          'restart_resume': True, 'program_migration_completed': False, 'report': str(output)}))
    finally:
        resumed.close()


if __name__ == '__main__':
    main()
