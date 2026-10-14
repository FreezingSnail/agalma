import copy
import hashlib
import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parent
schema = json.loads((ROOT / 'schema.json').read_text())
objects = json.loads((ROOT / 'objects.json').read_text())

def canonical(v):
    return json.dumps(v, sort_keys=True, separators=(',', ':'), ensure_ascii=False).encode()

def digest(v):
    return 'sha256:' + hashlib.sha256(canonical(v)).hexdigest()

class Invalid(ValueError):
    pass

def check(s, v, path='$'):
    if '$ref' in s:
        parts = s['$ref'].split('/')[1:]
        target = schema
        for p in parts:
            target = target[p]
        return check(target, v, path)
    if 'oneOf' in s:
        matches = 0
        for arm in s['oneOf']:
            try:
                check(arm, v, path); matches += 1
            except Invalid:
                pass
        if matches != 1:
            raise Invalid(f'{path}: expected one schema arm, got {matches}')
        return
    if 'const' in s and (type(v) != type(s['const']) or v != s['const']):
        raise Invalid(f'{path}: const mismatch')
    if 'enum' in s and not any(type(v) == type(e) and v == e for e in s['enum']):
        raise Invalid(f'{path}: invalid enum')
    kind = s.get('type')
    types = {'object': dict, 'array': list, 'string': str, 'integer': int, 'boolean': bool, 'null': type(None)}
    if kind and type(v) is not types[kind]:
        raise Invalid(f'{path}: expected {kind}')
    if kind == 'object':
        missing = set(s.get('required', [])) - v.keys()
        if missing:
            raise Invalid(f'{path}: missing {sorted(missing)}')
        props = s.get('properties', {})
        for k, val in v.items():
            if k in props:
                check(props[k], val, path+'.'+k)
            elif s.get('additionalProperties') is False:
                raise Invalid(f'{path}: unexpected {k}')
            elif isinstance(s.get('additionalProperties'), dict):
                check(s['additionalProperties'], val, path+'.'+k)
    if kind == 'array':
        if len(v) < s.get('minItems', 0) or len(v) > s.get('maxItems', 2**63):
            raise Invalid(f'{path}: array length')
        for i, val in enumerate(v):
            check(s['items'], val, f'{path}[{i}]')
    if kind == 'integer' and not s.get('minimum', -2**64) <= v <= s.get('maximum', 2**64):
        raise Invalid(f'{path}: integer range')
    if kind == 'string':
        if len(v) < s.get('minLength', 0):
            raise Invalid(f'{path}: string length')
        if 'pattern' in s and not re.search(s['pattern'], v):
            raise Invalid(f'{path}: string pattern')

supported = {'$schema', '$id', '$defs', '$ref', 'title', 'oneOf', 'const', 'enum', 'type',
    'properties', 'required', 'additionalProperties', 'items', 'minItems', 'maxItems',
    'minimum', 'maximum', 'minLength', 'pattern'}
def audit_schema(s):
    assert not set(s) - supported, set(s) - supported
    if '$ref' in s:
        assert s['$ref'].startswith('#/$defs/')
        assert s['$ref'].split('/')[-1] in schema['$defs']
    for group in ('$defs', 'properties'):
        for child in s.get(group, {}).values():
            audit_schema(child)
    for child in s.get('oneOf', []):
        audit_schema(child)
    if 'items' in s:
        audit_schema(s['items'])
    if isinstance(s.get('additionalProperties'), dict):
        audit_schema(s['additionalProperties'])
audit_schema(schema)

def refs(value):
    if isinstance(value, dict):
        if {'path','digest','bytes','media_type'} <= value.keys():
            assert value['path'].startswith('objects/')
            text = objects[value['path'][8:]].encode()
            assert value['bytes'] == len(text)
            assert value['digest'] == 'sha256:' + hashlib.sha256(text).hexdigest()
        for child in value.values():
            refs(child)
    if isinstance(value, list):
        for child in value:
            refs(child)

excluded = {'schema.json', 'objects.json', 'invalid-records.json', 'canonical-vectors.json', 'snapshot-sections.json'}
records = {}
for p in sorted(ROOT.glob('*.json')):
    if p.name in excluded:
        continue
    value = json.loads(p.read_text())
    check(schema, value); refs(value); records[p.stem] = value
    fields = {'TaskPackage/v1':'package_digest','ComponentCommit/v1':'input_digest',
        'ProviderBudgetProfile/v1':'profile_digest','MutationCandidate/v1':'candidate_digest',
        'EvaluationPolicy/v1':'policy_digest','TaskReleasePolicy/v1':'policy_digest','PluginSnapshot/v1':'snapshot_digest',
        'PluginInstallation/v1':'installation_digest','EpicReviewAdmission/v1':'input_digest'}
    if value['format'] in fields:
        field = fields[value['format']]
        expected = value.pop(field)
        assert expected == digest(value), p
        value[field] = expected
    if value['format'] in ('ReportEnvelope/v1', 'ReportPublication/v1'):
        assert value['payload_digest'] == digest(value['payload'])
    if value['format'] == 'ActionIntent/v1':
        assert value['input_digest'] == digest(value['input'])
    if value['format'] == 'TaskPackage/v1':
        for field in ('description', 'design', 'notes'):
            item = value['definition'][field]
            assert item['digest'] == 'sha256:' + hashlib.sha256(item['text'].encode()).hexdigest()
        assert value['scope']['oracle']['digest'] not in value['scope']['readable_artifacts']
    if value['format'] == 'CancellationSettlement/v1':
        assert value['state'] != 'settled' or (not value['unconfirmed_operations'] and value['settled_at_unix_ms'] is not None)
    if value['format'] == 'EpicReviewAdmission/v1':
        blocked = sorted(e['target_id'] for e in value['edges'] if e['type'] == 'blocks' and e['observed_status'] != 'closed')
        assert sorted(value['blocking_ids']) == blocked
        if value['decision'] == 'eligible':
            assert value['children'] and not blocked
            assert all(c['status'] == 'closed' for c in value['children'])
    if value['format'] == 'PluginOperationStatus/v1':
        if value['state'] in ('completed','failed','cancelled'):
            assert value['receipt'] is not None
            assert value['receipt']['operation_id'] == value['operation_id']
            assert value['receipt']['input_digest'] == value['input_digest']

bad = json.loads((ROOT/'invalid-records.json').read_text())
for case in bad:
    try:
        check(schema, case['record'])
    except Invalid:
        continue
    raise AssertionError('negative record accepted: ' + case['name'])
vectors = json.loads((ROOT/'canonical-vectors.json').read_text())
for vector in vectors:
    assert vector['canonical_utf8'].encode() == canonical(vector['input'])
    assert vector['digest'] == digest(vector['input'])
provider = records['provider-budget-profile']
bound = (provider['max_billable_input_tokens'] * provider['input_cash_per_million_nusd']
    + provider['max_billable_output_tokens'] * provider['output_cash_per_million_nusd'] + 999999)//1000000
bound += provider['other_maximum_charge_nusd']
assert bound == 14336000
snapshot = json.loads((ROOT/'snapshot-sections.json').read_text())
source = (ROOT.parents[2]/'crates/agalma-contracts/src/snapshot.rs').read_text()
old_fields = set(re.findall(r'^    pub ([a-z_]+):', source, re.M)) - {'format', 'schema_version'}
assert old_fields <= set(snapshot['sections']), old_fields - set(snapshot['sections'])
assert {'decisions','decision_outcomes','digests','control_receipts','component_states',
    'component_events','component_operations','component_receipts','component_commits'} <= set(snapshot['sections'])
print(f'OK: {len(records)} positive records, {len(bad)} rejected records, {len(vectors)} canonical vectors; all refs/digests and fake reservation checked')
