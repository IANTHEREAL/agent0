#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# ///
"""
TiKV Cluster Administration Tool for pg-tikv Testing

Manages TiKV test clusters using tiup playground with API v2 (keyspace support).

Usage:
    tikv_admin.py start [--name NAME] [--persistent] [--pd-port PORT] [--host HOST]
    tikv_admin.py stop [--name NAME] [--all]
    tikv_admin.py list
    tikv_admin.py status [--name NAME]
    tikv_admin.py clean [--name NAME] [--all]

Modes:
    One-time (default): Cluster auto-cleans up when tests finish
    Persistent (--persistent): Cluster keeps running for development

Examples:
    tikv_admin.py start                           # Start default cluster (one-time mode)
    tikv_admin.py start --name dev --persistent   # Start persistent dev cluster
    tikv_admin.py start --name ci                 # Start one-time CI cluster
    tikv_admin.py start --host 0.0.0.0            # Start with PD/TiKV listening on all interfaces
    tikv_admin.py list                            # List all managed clusters
    tikv_admin.py stop --name dev                 # Stop specific cluster
    tikv_admin.py stop --all                      # Stop all clusters
    tikv_admin.py clean --all                     # Clean all cluster data
"""

import subprocess
import sys
import os
import json
import time
import signal
import socket
import re
import argparse
import shutil
import atexit
from pathlib import Path
from dataclasses import dataclass, asdict
from typing import Optional, List, Dict
from datetime import datetime
from enum import Enum

ADMIN_DIR = Path.home() / ".pg-tikv"
CLUSTERS_DIR = ADMIN_DIR / "clusters"
LOCK_FILE = ADMIN_DIR / ".lock"
DEFAULT_CLUSTER_NAME = "default"

GREEN = "\033[0;32m"
YELLOW = "\033[1;33m"
RED = "\033[0;31m"
BLUE = "\033[0;34m"
CYAN = "\033[0;36m"
NC = "\033[0m"


class ClusterMode(Enum):
    ONE_TIME = "one-time"
    PERSISTENT = "persistent"


@dataclass
class ClusterInfo:
    name: str
    mode: str
    pd_port: int
    tikv_port: int
    pid: int
    created_at: str
    data_dir: str
    log_file: str
    host: str = "127.0.0.1"
    status: str = "running"

    def to_dict(self) -> dict:
        return asdict(self)

    @classmethod
    def from_dict(cls, data: dict) -> "ClusterInfo":
        return cls(**data)


def log_info(msg: str):
    print(f"{GREEN}[INFO]{NC} {msg}")


def log_warn(msg: str):
    print(f"{YELLOW}[WARN]{NC} {msg}")


def log_error(msg: str):
    print(f"{RED}[ERROR]{NC} {msg}")


def log_debug(msg: str, verbose: bool = False):
    if verbose:
        print(f"{BLUE}[DEBUG]{NC} {msg}")


def ensure_dirs():
    """Ensure admin directories exist."""
    ADMIN_DIR.mkdir(parents=True, exist_ok=True)
    CLUSTERS_DIR.mkdir(parents=True, exist_ok=True)


def check_tiup() -> bool:
    """Check if tiup is installed."""
    result = subprocess.run(["which", "tiup"], capture_output=True)
    if result.returncode != 0:
        log_error("tiup is not installed. Install with:")
        log_error("  curl --proto '=https' --tlsv1.2 -sSf https://tiup-mirrors.pingcap.com/install.sh | sh")
        return False
    return True


def find_free_port(start: int = 2379, end: int = 2479) -> Optional[int]:
    """Find an available port in the given range."""
    for port in range(start, end):
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
            sock.settimeout(0.1)
            if sock.connect_ex(("127.0.0.1", port)) != 0:
                return port
    return None


def wait_for_port(host: str, port: int, timeout: int = 60) -> bool:
    """Wait for a port to become available."""
    for _ in range(timeout):
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
            sock.settimeout(1)
            if sock.connect_ex((host, port)) == 0:
                return True
        time.sleep(1)
    return False


def is_port_in_use(port: int) -> bool:
    """Check if a port is in use."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.settimeout(0.1)
        return sock.connect_ex(("127.0.0.1", port)) == 0


def get_cluster_dir(name: str) -> Path:
    """Get the directory for a cluster."""
    return CLUSTERS_DIR / name


def get_cluster_info_file(name: str) -> Path:
    """Get the cluster info file path."""
    return get_cluster_dir(name) / "cluster.json"


def save_cluster_info(info: ClusterInfo):
    """Save cluster info to disk."""
    cluster_dir = get_cluster_dir(info.name)
    cluster_dir.mkdir(parents=True, exist_ok=True)
    info_file = get_cluster_info_file(info.name)
    with open(info_file, "w") as f:
        json.dump(info.to_dict(), f, indent=2)


def load_cluster_info(name: str) -> Optional[ClusterInfo]:
    """Load cluster info from disk."""
    info_file = get_cluster_info_file(name)
    if not info_file.exists():
        return None
    try:
        with open(info_file) as f:
            data = json.load(f)
            return ClusterInfo.from_dict(data)
    except (json.JSONDecodeError, KeyError):
        return None


def list_clusters() -> List[ClusterInfo]:
    """List all managed clusters."""
    clusters = []
    if not CLUSTERS_DIR.exists():
        return clusters
    
    for cluster_dir in CLUSTERS_DIR.iterdir():
        if cluster_dir.is_dir():
            info = load_cluster_info(cluster_dir.name)
            if info:
                info.status = check_cluster_process(info)
                clusters.append(info)
    return clusters


def check_cluster_process(info: ClusterInfo) -> str:
    """Check if the cluster process is still running."""
    try:
        os.kill(info.pid, 0)
        if is_port_in_use(info.pd_port):
            return "running"
        return "starting"
    except (OSError, ProcessLookupError):
        return "stopped"


def extract_ports_from_log(log_file: Path, timeout: int = 120) -> Optional[Dict[str, int]]:
    """Extract PD and TiKV ports from tiup playground log."""
    pd_pattern = re.compile(r"PD (?:Endpoints|client).*?[\d\.]+:(\d+)")
    tikv_pattern = re.compile(r"TiKV.*?[\d\.]+:(\d+)")
    
    ports = {}
    start_time = time.time()
    
    while time.time() - start_time < timeout:
        if log_file.exists():
            content = log_file.read_text()
            
            if "PD" not in ports:
                match = pd_pattern.search(content)
                if match:
                    ports["pd"] = int(match.group(1))
            
            if "tikv" not in ports:
                match = tikv_pattern.search(content)
                if match:
                    ports["tikv"] = int(match.group(1))
            
            if "pd" in ports:
                return ports
        
        time.sleep(1)
    
    return ports if ports else None


def start_cluster(
    name: str = DEFAULT_CLUSTER_NAME,
    mode: ClusterMode = ClusterMode.ONE_TIME,
    pd_port: Optional[int] = None,
    host: str = "127.0.0.1",
    verbose: bool = False,
) -> Optional[ClusterInfo]:
    """Start a new TiKV cluster."""
    ensure_dirs()
    
    existing = load_cluster_info(name)
    if existing and check_cluster_process(existing) == "running":
        log_warn(f"Cluster '{name}' is already running (PD: {existing.pd_port})")
        return existing
    
    if not check_tiup():
        return None
    
    cluster_dir = get_cluster_dir(name)
    cluster_dir.mkdir(parents=True, exist_ok=True)
    
    tikv_config = cluster_dir / "tikv.toml"
    tikv_config.write_text("""\
[storage]
api-version = 2
enable-ttl = true
""")
    
    log_file = cluster_dir / "playground.log"
    data_dir = cluster_dir / "data"
    data_dir.mkdir(exist_ok=True)
    
    cmd = [
        "tiup", "playground",
        "--mode", "tikv-slim",
        "--kv.config", str(tikv_config),
        "--tag", f"pg-tikv-{name}",
        "--pd.host", host,
        "--kv.host", host,
    ]
    
    if pd_port:
        cmd.extend(["--pd.port", str(pd_port)])
    
    log_info(f"Starting TiKV cluster '{name}' ({mode.value} mode)...")
    log_debug(f"Command: {' '.join(cmd)}", verbose)
    
    with open(log_file, "w") as f:
        proc = subprocess.Popen(
            cmd,
            stdout=f,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
    
    log_info(f"Cluster process started (PID: {proc.pid})")
    log_info("Waiting for PD to be ready...")
    
    ports = extract_ports_from_log(log_file, timeout=120)
    if not ports or "pd" not in ports:
        log_error("Failed to start cluster - could not detect PD port")
        log_error(f"Check log file: {log_file}")
        proc.terminate()
        return None
    
    pd_port_actual = ports["pd"]
    tikv_port_actual = ports.get("tikv", 20160)
    
    check_host = "127.0.0.1" if host == "0.0.0.0" else host
    if not wait_for_port(check_host, pd_port_actual, 60):
        log_error(f"PD port {pd_port_actual} is not accessible")
        proc.terminate()
        return None
    
    log_info(f"Cluster '{name}' is ready!")
    log_info(f"  PD endpoint: {host}:{pd_port_actual}")
    log_info(f"  TiKV: {host}:{tikv_port_actual}")
    
    info = ClusterInfo(
        name=name,
        mode=mode.value,
        pd_port=pd_port_actual,
        tikv_port=tikv_port_actual,
        pid=proc.pid,
        created_at=datetime.now().isoformat(),
        data_dir=str(data_dir),
        log_file=str(log_file),
        host=host,
        status="running",
    )
    save_cluster_info(info)
    
    return info


def stop_cluster(name: str, force: bool = False) -> bool:
    """Stop a running cluster."""
    info = load_cluster_info(name)
    if not info:
        log_warn(f"Cluster '{name}' not found")
        return False
    
    status = check_cluster_process(info)
    if status == "stopped":
        log_info(f"Cluster '{name}' is already stopped")
        return True
    
    log_info(f"Stopping cluster '{name}' (PID: {info.pid})...")
    
    try:
        os.kill(info.pid, signal.SIGTERM)
        
        for _ in range(10):
            try:
                os.kill(info.pid, 0)
                time.sleep(1)
            except OSError:
                break
        
        try:
            os.kill(info.pid, 0)
            log_warn("Process didn't exit gracefully, force killing...")
            os.kill(info.pid, signal.SIGKILL)
        except OSError:
            pass
        
    except (OSError, ProcessLookupError):
        pass
    
    subprocess.run(
        ["tiup", "clean", f"pg-tikv-{name}"],
        capture_output=True,
    )
    
    info.status = "stopped"
    save_cluster_info(info)
    
    log_info(f"Cluster '{name}' stopped")
    return True


def stop_all_clusters() -> int:
    """Stop all managed clusters."""
    clusters = list_clusters()
    stopped = 0
    for cluster in clusters:
        if cluster.status == "running":
            if stop_cluster(cluster.name):
                stopped += 1
    return stopped


def clean_cluster(name: str) -> bool:
    """Clean cluster data and remove from management."""
    stop_cluster(name)
    
    cluster_dir = get_cluster_dir(name)
    if cluster_dir.exists():
        log_info(f"Removing cluster data: {cluster_dir}")
        shutil.rmtree(cluster_dir)
        return True
    return False


def clean_all_clusters() -> int:
    """Clean all cluster data."""
    if not CLUSTERS_DIR.exists():
        return 0
    
    cleaned = 0
    for cluster_dir in list(CLUSTERS_DIR.iterdir()):
        if cluster_dir.is_dir():
            clean_cluster(cluster_dir.name)
            cleaned += 1
    return cleaned


def print_cluster_status(info: ClusterInfo, detailed: bool = False):
    """Print cluster status."""
    status_color = GREEN if info.status == "running" else YELLOW if info.status == "starting" else RED
    mode_str = f"[{info.mode}]"
    host = getattr(info, 'host', '127.0.0.1')
    
    print(f"  {CYAN}{info.name}{NC} {status_color}({info.status}){NC} {mode_str}")
    print(f"    PD: {host}:{info.pd_port}")
    print(f"    TiKV: {host}:{info.tikv_port}")
    print(f"    PID: {info.pid}")
    
    if detailed:
        print(f"    Created: {info.created_at}")
        print(f"    Data: {info.data_dir}")
        print(f"    Log: {info.log_file}")


def cmd_start(args):
    """Handle start command."""
    mode = ClusterMode.PERSISTENT if args.persistent else ClusterMode.ONE_TIME
    info = start_cluster(
        name=args.name,
        mode=mode,
        pd_port=args.pd_port,
        host=args.host,
        verbose=args.verbose,
    )
    if info:
        print()
        print(f"Cluster '{args.name}' is ready.")
        print(f"  PD_ENDPOINTS={info.host}:{info.pd_port}")
        print()
        if mode == ClusterMode.ONE_TIME:
            print(f"Mode: {YELLOW}one-time{NC} - cluster will be managed by caller")
        else:
            print(f"Mode: {GREEN}persistent{NC} - stop with: tikv_admin.py stop --name {args.name}")
        return 0
    return 1


def cmd_stop(args):
    """Handle stop command."""
    if args.all:
        stopped = stop_all_clusters()
        log_info(f"Stopped {stopped} cluster(s)")
        return 0
    
    if stop_cluster(args.name, force=args.force):
        return 0
    return 1


def cmd_list(args):
    """Handle list command."""
    clusters = list_clusters()
    
    if not clusters:
        print("No managed clusters found")
        return 0
    
    print(f"Managed TiKV Clusters ({len(clusters)}):")
    print("-" * 50)
    
    for cluster in clusters:
        print_cluster_status(cluster, detailed=args.verbose)
        print()
    
    return 0


def cmd_status(args):
    """Handle status command."""
    info = load_cluster_info(args.name)
    if not info:
        log_error(f"Cluster '{args.name}' not found")
        return 1
    
    info.status = check_cluster_process(info)
    save_cluster_info(info)
    
    print(f"Cluster Status: {args.name}")
    print("-" * 40)
    print_cluster_status(info, detailed=True)
    
    return 0


def cmd_clean(args):
    """Handle clean command."""
    if args.all:
        cleaned = clean_all_clusters()
        log_info(f"Cleaned {cleaned} cluster(s)")
        return 0
    
    if clean_cluster(args.name):
        log_info(f"Cleaned cluster '{args.name}'")
        return 0
    return 1


def main():
    parser = argparse.ArgumentParser(
        description="TiKV Cluster Administration Tool for pg-tikv Testing",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("-v", "--verbose", action="store_true", help="Verbose output")
    
    subparsers = parser.add_subparsers(dest="command", help="Commands")
    
    start_parser = subparsers.add_parser("start", help="Start a TiKV cluster")
    start_parser.add_argument("--name", default=DEFAULT_CLUSTER_NAME, help="Cluster name")
    start_parser.add_argument("--persistent", action="store_true", help="Keep cluster running (persistent mode)")
    start_parser.add_argument("--pd-port", type=int, help="PD port (auto-assigned if not specified)")
    start_parser.add_argument("--host", default="127.0.0.1", help="Host for PD and TiKV to bind (default: 127.0.0.1, use 0.0.0.0 for all interfaces)")
    start_parser.add_argument("-v", "--verbose", action="store_true", help="Verbose output")
    
    stop_parser = subparsers.add_parser("stop", help="Stop a TiKV cluster")
    stop_parser.add_argument("--name", default=DEFAULT_CLUSTER_NAME, help="Cluster name")
    stop_parser.add_argument("--all", action="store_true", help="Stop all clusters")
    stop_parser.add_argument("-f", "--force", action="store_true", help="Force stop")
    
    list_parser = subparsers.add_parser("list", help="List all managed clusters")
    list_parser.add_argument("-v", "--verbose", action="store_true", help="Verbose output")
    
    status_parser = subparsers.add_parser("status", help="Show cluster status")
    status_parser.add_argument("--name", default=DEFAULT_CLUSTER_NAME, help="Cluster name")
    
    clean_parser = subparsers.add_parser("clean", help="Clean cluster data")
    clean_parser.add_argument("--name", default=DEFAULT_CLUSTER_NAME, help="Cluster name")
    clean_parser.add_argument("--all", action="store_true", help="Clean all clusters")
    
    args = parser.parse_args()
    
    if not args.command:
        parser.print_help()
        return 1
    
    commands = {
        "start": cmd_start,
        "stop": cmd_stop,
        "list": cmd_list,
        "status": cmd_status,
        "clean": cmd_clean,
    }
    
    return commands[args.command](args)


if __name__ == "__main__":
    sys.exit(main())
