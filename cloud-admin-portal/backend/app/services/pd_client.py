"""TiKV PD (Placement Driver) client for keyspace management."""

from typing import List, Optional
import requests


class PDClient:
    """Client for TiKV Placement Driver HTTP API.
    
    Manages keyspaces (tenant isolation units) in TiKV cluster.
    """
    
    def __init__(self, endpoints: str):
        """Initialize PD client.
        
        Args:
            endpoints: Comma-separated PD addresses (e.g., "127.0.0.1:2379")
        """
        self.endpoints = [e.strip() for e in endpoints.split(",")]
        self.base_url = f"http://{self.endpoints[0]}"
        self._timeout = 10
    
    def create_keyspace(self, name: str) -> bool:
        """Create a new keyspace.

        Args:
            name: Keyspace name

        Returns:
            True if created successfully or already exists
        """
        url = f"{self.base_url}/pd/api/v2/keyspaces"
        try:
            resp = requests.post(url, json={"name": name}, timeout=self._timeout)
            print(f"[DEBUG] Creating keyspace '{name}': POST {url}")
            print(f"[DEBUG] Response status: {resp.status_code}")
            print(f"[DEBUG] Response body: {resp.text}")

            if resp.status_code == 200:
                return True
            if "already exists" in resp.text.lower():
                return True

            print(f"[ERROR] Failed to create keyspace: {resp.status_code} - {resp.text}")
            return False
        except requests.RequestException as e:
            print(f"[ERROR] Request exception when creating keyspace: {e}")
            return False
    
    def list_keyspaces(self) -> List[dict]:
        """List all keyspaces.
        
        Returns:
            List of keyspace info dicts
        """
        url = f"{self.base_url}/pd/api/v2/keyspaces"
        try:
            resp = requests.get(url, timeout=self._timeout)
            if resp.status_code == 200:
                data = resp.json()
                return data.get("keyspaces", [])
            return []
        except requests.RequestException:
            return []
    
    def get_keyspace(self, name: str) -> Optional[dict]:
        """Get keyspace details.

        Args:
            name: Keyspace name

        Returns:
            Keyspace info dict or None
        """
        url = f"{self.base_url}/pd/api/v2/keyspaces/{name}"
        try:
            resp = requests.get(url, timeout=self._timeout)
            print(f"[DEBUG] Getting keyspace '{name}': GET {url}")
            print(f"[DEBUG] Response status: {resp.status_code}")

            if resp.status_code == 200:
                data = resp.json()
                print(f"[DEBUG] Keyspace data: {data}")
                return data
            print(f"[DEBUG] Keyspace '{name}' not found (status {resp.status_code})")
            return None
        except requests.RequestException as e:
            print(f"[ERROR] Request exception when getting keyspace: {e}")
            return None
    
    def disable_keyspace(self, name: str) -> bool:
        """Disable a keyspace (TiKV keyspaces cannot be fully deleted).
        
        Args:
            name: Keyspace name
        
        Returns:
            True if disabled successfully
        """
        url = f"{self.base_url}/pd/api/v2/keyspaces/{name}/state"
        try:
            resp = requests.put(
                url,
                json={"state": "DISABLED"},
                timeout=self._timeout
            )
            return resp.status_code in (200, 204)
        except requests.RequestException:
            return False
    
    def check_health(self) -> bool:
        """Check if PD is healthy.
        
        Returns:
            True if PD responds to health check
        """
        url = f"{self.base_url}/pd/api/v1/version"
        try:
            resp = requests.get(url, timeout=2)
            return resp.status_code == 200
        except requests.RequestException:
            return False
