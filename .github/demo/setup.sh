#!/bin/sh
# Builds a throwaway repository for the README demo: a bare remote, one real check, and bay installed in a
# private HOME. Usage: setup.sh <empty-dir>. Needs bay and git on PATH.
set -eu
D=$1; mkdir -p "$D/home/.local/bin"
export HOME="$D/home"; unset CLAUDE_CONFIG_DIR; export PATH="$HOME/.local/bin:$PATH"
cp "$(command -v bay)" "$HOME/.local/bin/"
git config --global user.name "Ada"; git config --global user.email ada@example.com; git config --global init.defaultBranch main
cd "$D"; git init -q --bare remote.git; git clone -q remote.git app 2>/dev/null; cd app
mkdir -p .bay src scripts
: > .bay/config.toml
cat > .bay/policy.toml <<'EOF'
revision = 1

[[check]]
id = "greeting"
environment = "local"
command = "scripts/bay-check"
adapter = ["scripts/bay-check"]
expected_minimum = 1
EOF
cat > scripts/bay-check <<'EOF'
#!/bin/sh
# One test: the greeting is not empty.
req=$(cat)
commit=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"}.*/\1/p')
tree=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"}.*/\2/p')
if grep -q . src/greeting.txt; then v=pass; else v=product_failure; fi
printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"discovered":1,"selected":1,"completed":1},"checks":[{"id":"greeting-not-empty","verdict":"%s"}]}' "$commit" "$tree" "$v"
EOF
chmod +x scripts/bay-check
echo "hello" > src/greeting.txt
git add -A; git commit -qm "start"; git push -q origin main 2>/dev/null
"$HOME/.local/bin/bay" install --trunk main --remote origin --json >/dev/null 2>&1
