# Tempo (Chain ID: 4217) 部署文档

## 特殊说明

- 写节点是 Rust 实现（非 go-ethereum fork），**不支持 vmtrace pipeline**，需通过 background-tracer ETL fetch 模式拉取 trace
- Leafage 需要 `--evm-type=tempo` 和 `--genesis-number=0`
- Leafage 依赖 writer + ETL 先启动并产出 Kafka 消息，否则启动时会 panic
- 启动顺序：写节点 -> ETL -> 读节点 -> 一致性校验

---

## 写节点 nodex-writer 部署

写节点 + ETL 两个服务配合工作。ETL 使用 `network_mode: "host"` 直连写节点 RPC/WS。

```yaml
services:
  tempo:
    container_name: tempo-mainnet
    image: 294354037686.dkr.ecr.ap-northeast-1.amazonaws.com/blockchain/tempo:v1.5.1-debank
    restart: unless-stopped
    stop_grace_period: 60s
    ports:
      - "8545:8545"
      - "8546:8546"
      - "9001:9001"
    volumes:
      - /data/tempo-writer:/data
    command:
      - node
      - --datadir=/data
      - --chain=mainnet
      - --follow=auto
      - --http
      - --http.addr=0.0.0.0
      - --http.api=eth,net,web3,trace,debug
      - --ws
      - --ws.addr=0.0.0.0
      - --metrics=0.0.0.0:9001
    environment:
      - RUST_LOG=info
      - RUST_BACKTRACE=1
    logging:
      driver: "json-file"
      options:
        max-size: "100m"
        max-file: "3"
        compress: "true"

  etl:
    container_name: etl-tempo
    image: 294354037686.dkr.ecr.ap-northeast-1.amazonaws.com/background-tracer:amd64-d1e468e
    network_mode: "host"
    user: "0:0"
    stop_signal: SIGTERM
    restart: unless-stopped
    environment:
      - RUST_LOG=info
      - RUST_BACKTRACE=1
    entrypoint:
      - /app/etl
    command:
      - fetch
      - --rpc-addr=http://127.0.0.1:8545
      - --ws-addr=ws://127.0.0.1:8546
      - --region=ap-northeast-1
      - --nodex-bucket=chaintable-nodex-pipeline--apne1-az4--x-s3
      - --chain-table-bucket=chaintable-pipeline--apne1-az4--x-s3
      - --brokers=b-2.chaintablenodexpi.udy5cj.c4.kafka.ap-northeast-1.amazonaws.com:9092
      - --topic=nodex_pipeline_4217_f490914c
      - --version=f490914c
      - --max-fork-depth=256
      - --max-task-queue-size=128
      - --poll-interval=100
    logging:
      driver: "json-file"
      options:
        max-size: "10m"
        max-file: "3"
```

数据卷 Volume: https://ap-northeast-1.console.aws.amazon.com/ec2/home?region=ap-northeast-1#VolumeDetails:volumeId=vol-058b9d9154e841f1b

快照: https://ap-northeast-1.console.aws.amazon.com/ec2/home?region=ap-northeast-1#SnapshotDetails:snapshotId=snap-0fb5972dedc1e7881

---

## 读节点 rpc 副本部署

注意 etcd-config 的 endpoints 是 etcd 地址，meta 是 nodex 自身的地址，需要能被一致性 check 节点和网关集群访问。

```yaml
services:
  leafage-evm-x-tempo:
    image: 294354037686.dkr.ecr.ap-northeast-1.amazonaws.com/leafage-evm-x:amd64-chaintable-v102-debank-18
    container_name: leafage-evm-x-tempo
    restart: unless-stopped
    network_mode: "host"
    environment:
      - RUST_LOG=info
      - RUST_BACKTRACE=1
    volumes:
      - /data/tempo-nodex:/nodex
    command:
      - standalone
      - --db-path=/nodex
      - --listen-addr=0.0.0.0:8536
      - --chain-cfg=4217
      - --evm-type=tempo
      - --archive
      - --meta=<本机IP>:8536
      - --kafka-s3-config={"topic":"nodex_pipeline_4217_f490914c","brokers":"b-2.chaintablenodexpi.udy5cj.c4.kafka.ap-northeast-1.amazonaws.com:9092","partition":0,"bucket_name":"chaintable-nodex-pipeline--apne1-az4--x-s3","outer_bucket_name":"chaintable-pipeline--apne1-az4--x-s3","offset_dir":"/nodex/offset","s3_chain_id":"4217","version":"f490914c"}
      - --etcd-config={"endpoints":["127.0.0.1:2379","127.0.0.1:2479","127.0.0.1:2579"],"keep_alive_interval_ms":500,"lease_ttl_s":5}
      - --genesis-number=0
    logging:
      driver: "json-file"
      options:
        max-size: "100m"
        max-file: "3"
        compress: "true"
```

数据卷 Volume: https://ap-northeast-1.console.aws.amazon.com/ec2/home?region=ap-northeast-1#VolumeDetails:volumeId=vol-07231e9e31faecdeb

快照: https://ap-northeast-1.console.aws.amazon.com/ec2/home?region=ap-northeast-1#SnapshotDetails:snapshotId=snap-090a64f4c5842b78d

---

## consistency-checker 一致性节点部署

```yaml
services:
  consistency-checker-tempo:
    image: 294354037686.dkr.ecr.ap-northeast-1.amazonaws.com/consistency-checkerx:amd64-v1.0.18
    network_mode: "host"
    container_name: consistency-tempo
    restart: always
    volumes:
      - /data/tempo-consistency:/tempo
      - ./config:/config
    command: ["-config", "/config/config.yml"]
    logging:
      driver: "json-file"
      options:
        max-size: "10m"
        max-file: "3"
```

config.yml

```yaml
listen: "0.0.0.0:8886"
ready_ratio: 0.8
check_num: 3
check_interval_ms: 20
chain_id: 4217
version: "f490914c"
consistency_db_path: "/tempo/consistency_db"
outer_s3_bucket: "chaintable-pipeline--apne1-az4--x-s3"
outer_s3_region: "ap-northeast-1"
inner_brokers:
  - "b-2.chaintablenodexpi.udy5cj.c4.kafka.ap-northeast-1.amazonaws.com:9092"
inner_new_block_topic: "nodex_pipeline_4217_f490914c"
inner_new_block_group_id: "consistency-group-4217-f490914c"
outer_brokers:
  - "b-2.chaintablenodexpi.udy5cj.c4.kafka.ap-northeast-1.amazonaws.com:9092"
outer_new_block_topic: "pipeline_4217"
outer_version_new_block_topic: "pipeline_4217_f490914c"
etcd_endpoints:
  - "127.0.0.1:2379"
  - "127.0.0.1:2479"
  - "127.0.0.1:2579"
available_nodes_ttl: 5
```

数据卷 Volume: https://ap-northeast-1.console.aws.amazon.com/ec2/home?region=ap-northeast-1#VolumeDetails:volumeId=vol-05603ee2858e62877

快照: https://ap-northeast-1.console.aws.amazon.com/ec2/home?region=ap-northeast-1#SnapshotDetails:snapshotId=snap-0192de43e863a59c2
