#!/usr/bin/env python3
import argparse
import json
from pathlib import Path
import yaml


def normalize_src_path(path: str) -> str:
    p = path.replace('\\\\', '/')
    if '/src/' in p:
        return 'src/' + p.split('/src/', 1)[1]
    return p


def load_json(path: Path):
    return json.loads(path.read_text(encoding='utf-8'))


def calc_area_coverage(line_cov: dict, area_map: dict):
    area_stats = {name: {'covered': 0, 'total': 0, 'paths': cfg.get('paths', [])} for name, cfg in area_map['areas'].items()}

    for d in line_cov.get('data', []):
        for f in d.get('files', []):
            src_path = normalize_src_path(f.get('filename', ''))
            s = f.get('summary', {}).get('lines', {})
            covered = int(s.get('covered', 0))
            total = int(s.get('count', 0))
            if total <= 0:
                continue
            for area, cfg in area_map['areas'].items():
                for prefix in cfg.get('paths', []):
                    if src_path.startswith(prefix):
                        area_stats[area]['covered'] += covered
                        area_stats[area]['total'] += total
                        break

    rows = []
    for area, st in area_stats.items():
        total = st['total']
        ratio = (st['covered'] / total) if total else 0.0
        rows.append({
            'area': area,
            'covered_lines': st['covered'],
            'total_lines': total,
            'ratio': round(ratio, 4),
            'paths': st['paths'],
        })

    total_cov = sum(x['covered_lines'] for x in rows)
    total_lines = sum(x['total_lines'] for x in rows)
    return {
        'status': 'ok',
        'global_ratio': round((total_cov / total_lines) if total_lines else 0.0, 4),
        'global_covered_lines': total_cov,
        'global_total_lines': total_lines,
        'areas': sorted(rows, key=lambda x: x['area']),
    }


def calc_critical_path_coverage(path_map: dict, stmt_cov: dict, scn_cov: dict, area_cov: dict):
    covered_stmt = {(x['statement'], x['phase']) for x in stmt_cov.get('covered_cells', [])}
    covered_scn = {(x['orm'], x['capability']) for x in scn_cov.get('covered_cells', [])}
    area_ratio = {x['area']: x['ratio'] for x in area_cov.get('areas', [])}

    out = []
    for name, cfg in path_map.get('critical_paths', {}).items():
        missing_stmt = []
        for s, p in cfg.get('required_statement_cells', []) or []:
            if (s, p) not in covered_stmt:
                missing_stmt.append([s, p])

        missing_area = []
        required_areas = cfg.get('required_areas_any', []) or []
        if required_areas:
            if not any(area_ratio.get(a, 0.0) > 0 for a in required_areas):
                missing_area = required_areas

        missing_scn = []
        required_scn_any = cfg.get('required_scenario_cells_any', []) or []
        if required_scn_any:
            if not any((o, c) in covered_scn for o, c in required_scn_any):
                missing_scn = required_scn_any

        covered = not missing_stmt and not missing_area and not missing_scn
        out.append({
            'path': name,
            'desc': cfg.get('desc', ''),
            'covered': covered,
            'missing_statement_cells': missing_stmt,
            'missing_area_any': missing_area,
            'missing_scenario_any': missing_scn,
        })

    total = len(out)
    cov = sum(1 for x in out if x['covered'])
    return {
        'status': 'ok',
        'covered_paths': cov,
        'total_paths': total,
        'ratio': round((cov / total) if total else 0.0, 4),
        'paths': out,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--area-map', default='auto_testing/area_map.yaml')
    ap.add_argument('--path-map', default='auto_testing/path_map.yaml')
    ap.add_argument('--line-cov', default='artifacts/coverage/line_coverage.json')
    ap.add_argument('--stmt-cov', default='artifacts/coverage/statement_protocol_coverage.json')
    ap.add_argument('--scn-cov', default='artifacts/coverage/scenario_coverage.json')
    ap.add_argument('--out-area', default='artifacts/coverage/area_coverage.json')
    ap.add_argument('--out-path', default='artifacts/coverage/critical_path_coverage.json')
    args = ap.parse_args()

    area_map = yaml.safe_load(Path(args.area_map).read_text(encoding='utf-8'))
    path_map = yaml.safe_load(Path(args.path_map).read_text(encoding='utf-8'))
    line_cov = load_json(Path(args.line_cov))
    stmt_cov = load_json(Path(args.stmt_cov))
    scn_cov = load_json(Path(args.scn_cov))

    area_cov = calc_area_coverage(line_cov, area_map)
    path_cov = calc_critical_path_coverage(path_map, stmt_cov, scn_cov, area_cov)

    out_area = Path(args.out_area)
    out_area.parent.mkdir(parents=True, exist_ok=True)
    out_area.write_text(json.dumps(area_cov, ensure_ascii=False, indent=2), encoding='utf-8')

    out_path = Path(args.out_path)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(path_cov, ensure_ascii=False, indent=2), encoding='utf-8')


if __name__ == '__main__':
    main()
