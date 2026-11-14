# Auth Demo

A simple demonstration of session-based authentication in Clean Language using clean-server.

## Features

- **Session-based authentication** with cookies
- **Protected routes** that require authentication
- **Role-based access control** (admin vs. user)
- **Login/Logout flow** with session management

## Endpoints

| Method | Path        | Auth     | Description              |
|--------|-------------|----------|--------------------------|
| GET    | /health     | Public   | Health check             |
| POST   | /login      | Public   | Login with credentials   |
| POST   | /logout     | Required | Logout (destroy session) |
| GET    | /protected  | Required | Protected resource       |
| GET    | /admin      | Admin    | Admin-only resource      |
| GET    | /me         | Required | Get current user info    |

## Build & Run

```bash
# From the clean-server directory
~/.cleen/bin/cln compile examples/auth-demo/app.cln -o examples/auth-demo/app.wasm
cargo run --bin clean-server -- examples/auth-demo/app.wasm
```

## Testing

```bash
# Health check (public)
curl http://localhost:3000/health

# Try protected route without auth (should fail)
curl http://localhost:3000/protected

# Login as regular user
curl -X POST http://localhost:3000/login \
  -H "Content-Type: application/json" \
  -d '{"email":"test@example.com","password":"secret"}' \
  -c cookies.txt

# Access protected route with session cookie
curl http://localhost:3000/protected -b cookies.txt

# Get current user info
curl http://localhost:3000/me -b cookies.txt

# Try admin route (should fail - user is not admin)
curl http://localhost:3000/admin -b cookies.txt

# Logout
curl -X POST http://localhost:3000/logout -b cookies.txt

# Try protected route again (should fail - session destroyed)
curl http://localhost:3000/protected -b cookies.txt
```

### Testing Admin Access

```bash
# Login as admin
curl -X POST http://localhost:3000/login \
  -H "Content-Type: application/json" \
  -d '{"email":"admin@example.com","password":"admin123"}' \
  -c admin-cookies.txt

# Access admin route (should work)
curl http://localhost:3000/admin -b admin-cookies.txt
```

## Test Users

| Email               | Password  | Role  |
|---------------------|-----------|-------|
| test@example.com    | secret    | user  |
| admin@example.com   | admin123  | admin |

## How It Works

1. **Login** (`POST /login`):
   - Validates credentials
   - Creates a session with `_session_create(user_id, role, claims)`
   - Sets a session cookie automatically

2. **Protected Routes**:
   - Registered with `_http_route_protected(method, path, handler, required_role)`
   - Server validates session cookie before calling handler
   - Returns 401 if not authenticated, 403 if role mismatch

3. **Session Functions**:
   - `_session_create(user_id, role, claims)` - Create new session
   - `_session_get()` - Get current session data
   - `_session_destroy()` - Destroy session (logout)
   - `_auth_get_session()` - Get auth context as JSON
   - `_auth_require_auth()` - Check if authenticated (returns 1/0)
   - `_auth_require_role(role)` - Check if user has role (returns 1/0)

4. **Logout** (`POST /logout`):
   - Calls `_session_destroy()` to invalidate session
   - Clears the session cookie
