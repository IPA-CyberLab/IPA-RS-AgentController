# SSH Test Machine Requirements

このリポジトリの通常テストはこの環境で実行できますが、`docs/goal.md` の
受け入れ条件に近い確認には root 権限、Btrfs、systemd-nspawn、
systemd/machinectl が必要です。こちらの実行環境ではそれらが揃わないため、
実機または使い捨て VM を SSH で借りる場合は、このファイルの条件を満たす
環境を用意してください。

詳細なセットアップ手順とコマンドは `tests/environment-requirements.md`、
非破壊の事前チェックは `tests/check-privileged-environment.sh` にあります。

## この環境で足りないもの

現在の作業環境では privileged integration test を最後まで実行できません。
主な不足は次の通りです。

- `/` が Btrfs subvolume ではない
- `btrfs` コマンドと Btrfs quota が使えない
- `systemd-nspawn` と `machinectl` が使えない
- systemd machine を起動・停止できる root 権限付きホストではない

そのため、ここでは `cargo test`、`cargo clippy`、ignored test のビルド確認、
事前チェック script の構文確認までを行い、実際の nspawn/Btrfs を使う受け入れ
確認は SSH 先の実機または VM で実行します。

## 共有してほしい接続情報

- SSH ホスト名または IP アドレス
- SSH ユーザー名
- 認証方法
  - 公開鍵を登録する場合は、こちらが使う公開鍵の登録先ユーザー
  - パスワード認証の場合は一時パスワード
- root への昇格方法
  - root SSH が可能か
  - または `sudo` がパスワードなしで使えるか
- リポジトリを配置するディレクトリ
- 破壊的な検証を実行してよいことの確認
- 検証後にこちらで `/agentfs` と systemd 設定を片付けてよいか

この検証は `/agentfs`、`/etc/systemd/nspawn`、`/etc/systemd/network` に
状態を作成し、Btrfs サブボリュームや systemd-nspawn machine を作成・削除
します。普段使いの PC ではなく、使い捨て VM または専用検証機を推奨します。

## 必須 OS 条件

- Debian または Ubuntu 系 Linux
- PID 1 が systemd
- cgroup v2 が有効
- user namespace が有効
- `systemd-machined` と `systemd-networkd` が利用可能
- apt リポジトリと GitHub へ外向き通信できること
- `/` と `/agentfs` がどちらも Btrfs
- `/` が Btrfs subvolume
- `/agentfs` に 120 GiB 以上の空き容量があることを推奨

Ubuntu 22.04/24.04 または Debian 12 の使い捨て VM が最も扱いやすい想定です。
既存の普段使い環境を使う場合は、`/agentfs` や `af-codex-1`、
`af-claude-1` という名前の machine/subvolume が既存用途と衝突しないことを
事前に確認してください。

## 必須コマンド

ホスト上で次のコマンドが使える必要があります。

```text
apt または apt-get
btrfs
cargo
chroot
codex
dpkg
findmnt
git
machinectl
rustc
sudo
systemctl
systemd-nspawn
systemd-run
tee
tmux
```

また、`/bin/bash` が実行可能である必要があります。

## 事前インストール例

```bash
sudo apt update
sudo apt install -y \
  btrfs-progs \
  systemd-container \
  tmux \
  sudo \
  curl \
  git \
  build-essential \
  pkg-config
```

Rust は `cargo` と `rustc` が PATH から見える状態にしてください。

Codex CLI もホスト rootfs に必要です。検証中に `/` から base rootfs を作るため、
ホストに入っていないコマンドは child environment にも入りません。
Codex CLI は SSH で入ったユーザーからだけでなく、test を実行する root 環境の
`PATH` からも見える必要があります。

## こちらで実行するチェック

リポジトリを配置し、`agent-forkd` をインストール・起動した後、まず次を実行します。

```bash
sudo --preserve-env=PATH,CARGO_HOME,RUSTUP_HOME \
  env PATH="$PATH" \
  tests/check-privileged-environment.sh
```

その後、通常ゲートと privileged integration test を実行します。

```bash
cargo fmt -- --check
cargo test --quiet
cargo clippy --all-targets -- -D warnings
git diff --check
sudo --preserve-env=PATH,CARGO_HOME,RUSTUP_HOME \
  env PATH="$PATH" \
  cargo test -p agentctl --test privileged_sequence -- --ignored --nocapture
```

## 期待する最終状態

成功時は `codex-1` が削除され、`claude-1` が起動したまま残ります。
検証後に不要であれば、こちらで `claude-1`、`/agentfs` 配下の test state、
関連する systemd 設定を片付けます。
