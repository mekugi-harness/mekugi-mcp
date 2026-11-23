"""Google Cloud OAuth provider and email allowlist for FastMCP."""

from __future__ import annotations

import base64
import hashlib
import os
from pathlib import Path

from cryptography.fernet import Fernet
from fastmcp.server.auth.providers.google import GoogleProvider
from fastmcp.server.dependencies import get_access_token
from key_value.aio.stores.filetree import FileTreeStore
from key_value.aio.wrappers.encryption import FernetEncryptionWrapper


def _derive_fernet_key(secret: str) -> bytes:
    """Accept either a valid Fernet key or derive a deterministic 32-byte key."""
    raw = secret.encode("utf-8")
    try:
        Fernet(raw)
        return raw
    except Exception:
        digest = hashlib.sha256(raw).digest()
        return base64.urlsafe_b64encode(digest)


def parse_allowed_emails(raw: str | None) -> frozenset[str]:
    if not raw:
        return frozenset()
    return frozenset(email.strip().lower() for email in raw.split(",") if email.strip())


def build_google_auth_provider() -> GoogleProvider | None:
    """Build FastMCP GoogleProvider backed by encrypted FileTreeStore.

    Returns None when AUTH_DISABLED=true (for local testing).
    """
    if os.environ.get("AUTH_DISABLED", "").strip().lower() in ("1", "true", "yes"):
        return None

    client_id = os.environ.get("GOOGLE_CLIENT_ID", "").strip()
    client_secret = os.environ.get("GOOGLE_CLIENT_SECRET", "").strip()
    base_url = os.environ.get("BASE_URL", "").strip()
    allowed_emails = parse_allowed_emails(os.environ.get("ALLOWED_EMAILS"))
    allow_all = os.environ.get("ALLOW_ALL_GOOGLE_USERS", "").strip().lower() in ("1", "true", "yes")

    missing = [
        name
        for name, val in (
            ("GOOGLE_CLIENT_ID", client_id),
            ("GOOGLE_CLIENT_SECRET", client_secret),
            ("BASE_URL", base_url),
        )
        if not val
    ]
    if not allowed_emails and not allow_all:
        missing.append("ALLOWED_EMAILS")
    if missing:
        raise RuntimeError(
            f"Missing required Google OAuth environment variables: {', '.join(missing)}. "
            "Set them in .env (or ALLOW_ALL_GOOGLE_USERS=true / AUTH_DISABLED=true)."
        )

    jwt_signing_key = os.environ.get("JWT_SIGNING_KEY", "").strip() or client_secret
    storage_key_raw = os.environ.get("STORAGE_ENCRYPTION_KEY", "").strip() or jwt_signing_key
    storage_dir = Path(os.environ.get("OAUTH_STORAGE_DIR", "/data/oauth"))
    storage_dir.mkdir(parents=True, exist_ok=True)

    client_storage = FernetEncryptionWrapper(
        key_value=FileTreeStore(data_directory=storage_dir, auto_create=True),
        fernet=Fernet(_derive_fernet_key(storage_key_raw)),
    )

    consent_env = os.environ.get("OAUTH_REQUIRE_CONSENT", "remember").strip().lower()
    if consent_env in ("0", "false", "no"):
        require_consent: bool | str = False
    elif consent_env == "remember":
        require_consent = "remember"
    else:
        require_consent = True

    return GoogleProvider(
        client_id=client_id,
        client_secret=client_secret,
        base_url=base_url,
        required_scopes=[
            "openid",
            "https://www.googleapis.com/auth/userinfo.email",
        ],
        jwt_signing_key=jwt_signing_key,
        client_storage=client_storage,
        require_authorization_consent=require_consent,  # type: ignore[arg-type]
        extra_authorize_params={
            "access_type": "offline",
            "prompt": "consent",
        },
    )


def enforce_allowed_email(
    allowed_emails: frozenset[str],
    *,
    auth_enabled: bool = True,
    allow_all_users: bool = False,
) -> None:
    """Reject tool calls from Google accounts not in ALLOWED_EMAILS."""
    if not auth_enabled or allow_all_users:
        return
    if not allowed_emails:
        raise PermissionError("ALLOWED_EMAILS is empty; refusing access.")
    token = get_access_token()
    if token is None:
        raise PermissionError("Missing authenticated Google access token.")
    email = str(token.claims.get("email", "")).strip().lower()
    if email not in allowed_emails:
        raise PermissionError(f"Access denied for Google account: {email or 'unknown'}")
