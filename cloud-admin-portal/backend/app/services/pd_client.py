import logging
from typing import List, Optional

import requests

log = logging.getLogger(__name__)


class PDClient:
    def __init__(self, endpoints: str):
        self.endpoints = [e.strip() for e in endpoints.split(",")]
        self.base_url = f"http://{self.endpoints[0]}"
        self._timeout = 10

    def create_keyspace(self, name: str) -> bool:
        url = f"{self.base_url}/pd/api/v2/keyspaces"
        try:
            resp = requests.post(url, json={"name": name}, timeout=self._timeout)
            if resp.status_code == 200:
                return True
            if "already exists" in resp.text.lower():
                return True

            log.error("Failed to create keyspace %s: %s %s", name, resp.status_code, resp.text)
            return False
        except requests.RequestException as e:
            log.error("PD request error creating keyspace %s: %s", name, e)
            return False

    def list_keyspaces(self) -> List[dict]:
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
        url = f"{self.base_url}/pd/api/v2/keyspaces/{name}"
        try:
            resp = requests.get(url, timeout=self._timeout)
            if resp.status_code == 200:
                return resp.json()
            return None
        except requests.RequestException as e:
            log.error("PD request error getting keyspace %s: %s", name, e)
            return None

    def disable_keyspace(self, name: str) -> bool:
        url = f"{self.base_url}/pd/api/v2/keyspaces/{name}/state"
        try:
            resp = requests.put(url, json={"state": "DISABLED"}, timeout=self._timeout)
            return resp.status_code in (200, 204)
        except requests.RequestException:
            return False

    def check_health(self) -> bool:
        url = f"{self.base_url}/pd/api/v1/version"
        try:
            resp = requests.get(url, timeout=2)
            return resp.status_code == 200
        except requests.RequestException:
            return False
