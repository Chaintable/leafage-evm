// Generator for crates/leafage-evm-chains/tests/data/bsc_cas20/golden.json.
//
// It runs a CAS20 scenario through go-bsc's real state transition (core.ApplyMessage) on the
// BSC mainnet chain config with Jenner enabled, and records the pre-state, every
// transaction's outcome and the post-state, so the Rust port can replay the same
// transactions on BscEvm and compare byte for byte.
//
// Usage: copy this file into a go-bsc checkout at core/ (it is package core, since it needs
// ApplyMessage) and run
//
//	CAS20_GOLDEN_OUT=/path/to/golden.json go test ./core -run TestGenerateCAS20Golden -count=1
//
// Generated against go-bsc v1.8.1.
package core

import (
	"crypto/ecdsa"
	"encoding/json"
	"math/big"
	"os"
	"sort"
	"testing"

	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/common/hexutil"
	"github.com/ethereum/go-ethereum/core/state"
	"github.com/ethereum/go-ethereum/core/tracing"
	"github.com/ethereum/go-ethereum/core/types"
	"github.com/ethereum/go-ethereum/core/vm"
	"github.com/ethereum/go-ethereum/crypto"
	"github.com/ethereum/go-ethereum/params"
	"github.com/holiman/uint256"
)

// --- ABI encoding --------------------------------------------------------------------

func gw(n uint64) []byte { b := uint256.NewInt(n).Bytes32(); return b[:] }
func gwBig(n *uint256.Int) []byte { b := n.Bytes32(); return b[:] }
func ga(a common.Address) []byte { return common.LeftPadBytes(a.Bytes(), 32) }
func gh(h common.Hash) []byte { return h.Bytes() }
func gsel(sig string) []byte { return crypto.Keccak256([]byte(sig))[:4] }
func gkey(s string) common.Hash { return crypto.Keccak256Hash([]byte(s)) }

type gpart struct {
	word []byte
	tail []byte // dynamic when non-nil
}

func gstatic(w []byte) gpart { return gpart{word: w} }
func gbytes(b []byte) gpart {
	padded := (len(b) + 31) / 32 * 32
	t := append(gw(uint64(len(b))), make([]byte, padded)...)
	copy(t[32:], b)
	return gpart{tail: t}
}
func gstring(s string) gpart { return gbytes([]byte(s)) }
func gwords(ws ...[]byte) gpart {
	t := gw(uint64(len(ws)))
	for _, w := range ws {
		t = append(t, w...)
	}
	return gpart{tail: t}
}
func gbytesArray(elems ...[]byte) gpart {
	var bodies [][]byte
	for _, e := range elems {
		bodies = append(bodies, gbytes(e).tail)
	}
	t := gw(uint64(len(elems)))
	cur := uint64(32 * len(elems))
	for _, b := range bodies {
		t = append(t, gw(cur)...)
		cur += uint64(len(b))
	}
	for _, b := range bodies {
		t = append(t, b...)
	}
	return gpart{tail: t}
}
func gtuple(parts ...gpart) []byte {
	var head, tail []byte
	start := uint64(32 * len(parts))
	for _, p := range parts {
		if p.tail == nil {
			head = append(head, p.word...)
			continue
		}
		head = append(head, gw(start+uint64(len(tail)))...)
		tail = append(tail, p.tail...)
	}
	return append(head, tail...)
}
func gcall(sig string, parts ...gpart) []byte { return append(gsel(sig), gtuple(parts...)...) }

// --- the recording state -------------------------------------------------------------

type slotKey struct {
	addr common.Address
	slot common.Hash
}

type recorder struct{ slots map[slotKey]struct{} }

func (r *recorder) hooks() *tracing.Hooks {
	return &tracing.Hooks{
		OnStorageChange: func(addr common.Address, slot common.Hash, prev, new common.Hash) {
			r.slots[slotKey{addr, slot}] = struct{}{}
		},
	}
}

type goldenAccount struct {
	Balance *hexutil.Big                `json:"balance"`
	Nonce   hexutil.Uint64              `json:"nonce"`
	Code    hexutil.Bytes               `json:"code"`
	Storage map[common.Hash]common.Hash `json:"storage"`
}

type goldenLog struct {
	Address common.Address `json:"address"`
	Topics  []common.Hash  `json:"topics"`
	Data    hexutil.Bytes  `json:"data"`
}

type goldenTx struct {
	Name    string         `json:"name"`
	From    common.Address `json:"from"`
	To      common.Address `json:"to"`
	Input   hexutil.Bytes  `json:"input"`
	Gas     hexutil.Uint64 `json:"gas"`
	Value   *hexutil.Big   `json:"value"`
	Status  string         `json:"status"` // success | revert | halt
	GasUsed hexutil.Uint64 `json:"gasUsed"`
	Output  hexutil.Bytes  `json:"output"`
	Logs    []goldenLog    `json:"logs"`
}

type golden struct {
	ChainID        hexutil.Uint64                    `json:"chainId"`
	Number         hexutil.Uint64                    `json:"number"`
	Time           hexutil.Uint64                    `json:"time"`
	MilliTimestamp hexutil.Uint64                    `json:"milliTimestamp"`
	Coinbase       common.Address                    `json:"coinbase"`
	GasLimit       hexutil.Uint64                    `json:"gasLimit"`
	Pre            map[common.Address]*goldenAccount `json:"pre"`
	Txs            []goldenTx                        `json:"txs"`
	Post           map[common.Address]map[common.Hash]common.Hash `json:"post"`
}

// proxyCode: calldata = op(1) | target(20) | gas(8) | payload. op 0 CALL, 1 STATICCALL,
// 2 DELEGATECALL, 3 CALLCODE. Returns ok (32 bytes) ++ the callee's returndata.
func proxyCode() []byte {
	type ins struct {
		op    byte
		push  []byte
		label string // PUSH2 of a label
		dest  string // JUMPDEST label
	}
	var prog []ins
	op := func(o byte) { prog = append(prog, ins{op: o}) }
	push1 := func(v byte) { prog = append(prog, ins{op: 0x60, push: []byte{v}}) }
	pushL := func(l string) { prog = append(prog, ins{op: 0x61, label: l}) }
	dest := func(l string) { prog = append(prog, ins{op: 0x5b, dest: l}) }
	const (
		CALLDATASIZE, SUB, DUP1, DUP3, CALLDATACOPY, CALLDATALOAD, SHR, EQ, JUMPI, JUMP, POP = 0x36, 0x03, 0x80, 0x82, 0x37, 0x35, 0x1c, 0x14, 0x57, 0x56, 0x50
		CALL, STATICCALL, DELEGATECALL, CALLCODE, MSTORE, RETURNDATASIZE, RETURNDATACOPY, ADD, RETURN = 0xf1, 0xfa, 0xf4, 0xf2, 0x52, 0x3d, 0x3e, 0x01, 0xf3
	)
	target := func() { push1(1); op(CALLDATALOAD); push1(0x60); op(SHR) }
	gas := func() { push1(0x15); op(CALLDATALOAD); push1(0xc0); op(SHR) }
	push1(0x1d); op(CALLDATASIZE); op(SUB)
	op(DUP1); push1(0x1d); push1(0); op(CALLDATACOPY)
	push1(0); op(CALLDATALOAD); push1(0xf8); op(SHR)
	for i, l := range []string{"static", "delegate", "callcode"} {
		op(DUP1); push1(byte(i + 1)); op(EQ); pushL(l); op(JUMPI)
	}
	withValue := func(o byte) {
		op(POP)
		push1(0); push1(0); op(DUP3); push1(0); push1(0); target(); gas(); op(o)
		pushL("end"); op(JUMP)
	}
	withoutValue := func(o byte) {
		op(POP)
		push1(0); push1(0); op(DUP3); push1(0); target(); gas(); op(o)
		pushL("end"); op(JUMP)
	}
	withValue(CALL)
	dest("static"); withoutValue(STATICCALL)
	dest("delegate"); withoutValue(DELEGATECALL)
	dest("callcode"); withValue(CALLCODE)
	dest("end")
	push1(0); op(MSTORE); op(POP)
	op(RETURNDATASIZE); push1(0); push1(0x20); op(RETURNDATACOPY)
	op(RETURNDATASIZE); push1(0x20); op(ADD); push1(0); op(RETURN)

	labels := map[string]int{}
	pc := 0
	for _, in := range prog {
		if in.dest != "" {
			labels[in.dest] = pc
		}
		pc += 1 + len(in.push)
		if in.label != "" {
			pc += 2
		}
	}
	var code []byte
	for _, in := range prog {
		code = append(code, in.op)
		code = append(code, in.push...)
		if in.label != "" {
			p := labels[in.label]
			code = append(code, byte(p>>8), byte(p))
		}
	}
	return code
}

func proxyInput(op byte, target common.Address, gas uint64, payload []byte) []byte {
	out := []byte{op}
	out = append(out, target.Bytes()...)
	g := make([]byte, 8)
	new(big.Int).SetUint64(gas).FillBytes(g)
	out = append(out, g...)
	return append(out, payload...)
}

// --- the scenario --------------------------------------------------------------------

func erc7201(ns string) common.Hash {
	inner := new(uint256.Int).SetBytes(crypto.Keccak256([]byte(ns)))
	inner.SubUint64(inner, 1)
	b := inner.Bytes32()
	root := crypto.Keccak256Hash(b[:])
	root[31] = 0
	return root
}

func slotPlus(root common.Hash, off uint64) common.Hash {
	x := new(uint256.Int).SetBytes(root.Bytes())
	x.AddUint64(x, off)
	return x.Bytes32()
}

func TestGenerateCAS20Golden(t *testing.T) {
	out := os.Getenv("CAS20_GOLDEN_OUT")
	if out == "" {
		t.Skip("set CAS20_GOLDEN_OUT to generate")
	}

	cfg := *params.BSCChainConfig
	zero := uint64(0)
	cfg.JennerTime = &zero

	const (
		number = 100_000_000
		btime  = 1_790_000_000
		milli  = btime*1000 + 417
	)
	coinbase := common.HexToAddress("0xc0ffee0000000000000000000000000000c0ffee")

	statedb, _ := state.New(types.EmptyRootHash, state.NewDatabaseForTesting())
	rec := &recorder{slots: map[slotKey]struct{}{}}

	key, _ := crypto.HexToECDSA("4646464646464646464646464646464646464646464646464646464646464646")
	permitOwner := crypto.PubkeyToAddress(key.PublicKey)
	alice := common.HexToAddress("0x00000000000000000000000000000000000a11ce")
	bob := common.HexToAddress("0x0000000000000000000000000000000000000b0b")
	carol := common.HexToAddress("0x00000000000000000000000000000000000ca401")
	dave := common.HexToAddress("0x000000000000000000000000000000000000da7e")
	proxy := common.HexToAddress("0x0000000000000000000000000000000000900001")
	eoas := []common.Address{alice, bob, carol, dave, permitOwner}
	for _, a := range eoas {
		statedb.SetBalance(a, uint256.NewInt(1e18), tracing.BalanceChangeUnspecified)
	}
	statedb.SetCode(proxy, proxyCode(), tracing.CodeChangeUnspecified)

	// The fork seeds the registry sentinels; governance would appoint the admin and open
	// the features (BEP-702 3.15).
	vm.SeedCAS20Activation(statedb)
	actRoot := erc7201("bsc.activation_registry")
	statedb.SetState(vm.CAS20ActivationRegistryAddress, slotPlus(actRoot, 1), common.BytesToHash(alice.Bytes()))
	for _, f := range []string{"bsc.cas20_asset", "bsc.cas20_stablecoin", "bsc.policy_registry"} {
		slot := crypto.Keccak256Hash(gkey(f).Bytes(), slotPlus(actRoot, 0).Bytes())
		statedb.SetState(vm.CAS20ActivationRegistryAddress, slot, common.Hash{31: 1})
		rec.slots[slotKey{vm.CAS20ActivationRegistryAddress, slot}] = struct{}{}
	}
	rec.slots[slotKey{vm.CAS20ActivationRegistryAddress, slotPlus(actRoot, 1)}] = struct{}{}
	statedb.Finalise(true)

	blockCtx := vm.BlockContext{
		CanTransfer:    CanTransfer,
		Transfer:       Transfer,
		GetHash:        func(uint64) common.Hash { return common.Hash{} },
		Coinbase:       coinbase,
		GasLimit:       100_000_000,
		BlockNumber:    big.NewInt(number),
		Time:           btime,
		Difficulty:     big.NewInt(2),
		BaseFee:        big.NewInt(0),
		BlobBaseFee:    big.NewInt(1),
		Random:         &common.Hash{},
		MilliTimestamp: milli,
	}
	// The state processor, not NewEVM, applies tracing hooks to the state.
	hooks := rec.hooks()
	evm := vm.NewEVM(blockCtx, state.NewHookedState(statedb, hooks), &cfg, vm.Config{Tracer: hooks})
	gp := NewGasPool(1 << 62)
	txIndex := 0

	apply := func(from, to common.Address, input []byte, gas uint64, value *uint256.Int) (*ExecutionResult, []*types.Log) {
		if value == nil {
			value = new(uint256.Int)
		}
		msg := &Message{
			From: from, To: &to, Value: value, GasLimit: gas,
			GasPrice: new(uint256.Int), GasFeeCap: new(uint256.Int), GasTipCap: new(uint256.Int),
			Data: input, SkipNonceChecks: true,
		}
		hash := common.BigToHash(big.NewInt(int64(txIndex + 1)))
		statedb.SetTxContext(hash, txIndex)
		txIndex++
		evm.SetTxContext(NewEVMTxContext(msg))
		res, err := ApplyMessage(evm, msg, gp)
		if err != nil {
			t.Fatalf("apply %x: %v", input, err)
		}
		statedb.Finalise(true)
		return res, statedb.GetLogs(hash, number, common.Hash{}, btime)
	}
	mustOK := func(name string, res *ExecutionResult) []byte {
		if res.Failed() {
			t.Fatalf("setup %s failed: %v %x", name, res.Err, res.ReturnData)
		}
		return res.ReturnData
	}

	// --- setup: policies --------------------------------------------------------------
	reg := vm.CAS20PolicyRegistryAddress
	newPolicy := func(ptype byte, members ...common.Address) uint64 {
		var ws [][]byte
		for _, m := range members {
			ws = append(ws, ga(m))
		}
		ret := mustOK("policy", func() *ExecutionResult {
			r, _ := apply(alice, reg, gcall("createPolicyWithAccounts(address,uint8,address[])",
				gstatic(ga(alice)), gstatic(gw(uint64(ptype))), gwords(ws...)), 5_000_000, nil)
			return r
		}())
		return new(uint256.Int).SetBytes(ret).Uint64()
	}
	blockBob := newPolicy(0, bob)
	allowList := newPolicy(1, alice, carol, permitOwner)
	unionRet := mustOK("union", func() *ExecutionResult {
		r, _ := apply(alice, reg, gcall("createCompositePolicy(address,uint8,uint64[])",
			gstatic(ga(alice)), gstatic(gw(2)), gwords(gw(allowList), gw(blockBob))), 5_000_000, nil)
		return r
	}())
	union := new(uint256.Int).SetBytes(unionRet).Uint64()

	// --- setup: tokens ----------------------------------------------------------------
	roles := []string{"MINT_ROLE", "BURN_ROLE", "SEIZE_ROLE", "PAUSE_ROLE", "UNPAUSE_ROLE", "METADATA_ROLE"}
	initCalls := func(asset bool) [][]byte {
		var calls [][]byte
		all := roles
		if asset {
			all = append(append([]string{}, roles...), "OPERATOR_ROLE")
		}
		for _, r := range all {
			calls = append(calls, gcall("grantRole(bytes32,address)", gstatic(gh(gkey(r))), gstatic(ga(alice))))
		}
		calls = append(calls, gcall("mint(address,uint256)", gstatic(ga(alice)), gstatic(gwBig(uint256.NewInt(1_000_000)))))
		calls = append(calls, gcall("mint(address,uint256)", gstatic(ga(permitOwner)), gstatic(gwBig(uint256.NewInt(500)))))
		return calls
	}
	struct_ := func(members ...gpart) gpart { return gbytes(gtuple(gpart{tail: gtuple(members...)})) }
	assetParams := struct_(gstatic(gw(1)), gstring("CAS Asset Token"), gstring("CAT"), gstatic(ga(alice)), gstatic(gw(18)))
	stableParams := struct_(gstatic(gw(1)), gstring("CAS Stable Dollar Coin For The Golden Test"), gstring("CSD"), gstatic(ga(alice)), gstring("USD"))
	create := func(variant byte, salt common.Hash, p gpart, calls [][]byte) common.Address {
		ret := mustOK("create", func() *ExecutionResult {
			r, _ := apply(alice, vm.CAS20FactoryAddress, gcall("createCAS20(uint8,bytes32,bytes,bytes[])",
				gstatic(gw(uint64(variant))), gstatic(gh(salt)), p, gbytesArray(calls...)), 10_000_000, nil)
			return r
		}())
		return common.BytesToAddress(ret)
	}
	asset := create(0, common.Hash{31: 1}, assetParams, initCalls(true))
	stable := create(1, common.Hash{31: 2}, stableParams, initCalls(false))
	// An address in the CAS20 space nobody created: code-less, so not initialized.
	var uninit common.Address
	uninit[0], uninit[1], uninit[19] = 0xca, 0x52, 0x77

	// --- snapshot the pre-state -------------------------------------------------------
	g := &golden{
		ChainID: hexutil.Uint64(cfg.ChainID.Uint64()), Number: number, Time: btime,
		MilliTimestamp: milli, Coinbase: coinbase, GasLimit: 100_000_000,
		Pre: map[common.Address]*goldenAccount{}, Post: map[common.Address]map[common.Hash]common.Hash{},
	}
	accounts := append([]common.Address{asset, stable, uninit, proxy, coinbase, reg,
		vm.CAS20ActivationRegistryAddress, vm.CAS20FactoryAddress}, eoas...)
	snapshot := func() {
		for _, a := range accounts {
			g.Pre[a] = &goldenAccount{
				Balance: (*hexutil.Big)(statedb.GetBalance(a).ToBig()),
				Nonce:   hexutil.Uint64(statedb.GetNonce(a)),
				Code:    statedb.GetCode(a),
				Storage: map[common.Hash]common.Hash{},
			}
		}
		for k := range rec.slots {
			acc, ok := g.Pre[k.addr]
			if !ok {
				t.Fatalf("unlisted account %s has storage", k.addr)
			}
			if v := statedb.GetState(k.addr, k.slot); v != (common.Hash{}) {
				acc.Storage[k.slot] = v
			}
		}
	}
	snapshot()

	// --- the scenario -------------------------------------------------------------------
	record := func(name string, from, to common.Address, input []byte, gas uint64, value *uint256.Int) *ExecutionResult {
		res, logs := apply(from, to, input, gas, value)
		tx := goldenTx{Name: name, From: from, To: to, Input: input, Gas: hexutil.Uint64(gas),
			Value: (*hexutil.Big)(new(big.Int)), GasUsed: hexutil.Uint64(res.UsedGas), Output: res.ReturnData, Logs: []goldenLog{}}
		if value != nil {
			tx.Value = (*hexutil.Big)(value.ToBig())
		}
		switch {
		case res.Err == nil:
			tx.Status = "success"
		case res.Err == vm.ErrExecutionReverted:
			tx.Status = "revert"
		default:
			tx.Status = "halt"
		}
		for _, l := range logs {
			tx.Logs = append(tx.Logs, goldenLog{Address: l.Address, Topics: l.Topics, Data: l.Data})
		}
		g.Txs = append(g.Txs, tx)
		return res
	}
	tx := func(name string, from, to common.Address, input []byte) { record(name, from, to, input, 3_000_000, nil) }
	A, S := asset, stable
	u := func(n uint64) gpart { return gstatic(gw(n)) }
	addr := func(a common.Address) gpart { return gstatic(ga(a)) }
	h32 := func(h common.Hash) gpart { return gstatic(gh(h)) }
	maxU := new(uint256.Int).Not(new(uint256.Int))

	// views
	for _, tok := range []common.Address{A, S} {
		for _, sig := range []string{"name()", "symbol()", "decimals()", "totalSupply()", "contractURI()",
			"supplyCap()", "pausedFeatures()", "eip712Domain()", "DOMAIN_SEPARATOR()", "MINT_ROLE()",
			"DEFAULT_ADMIN_ROLE()", "TRANSFER_SENDER_POLICY()", "currency()", "multiplier()"} {
			tx("view "+sig, bob, tok, gsel(sig))
		}
		tx("balanceOf", bob, tok, gcall("balanceOf(address)", addr(alice)))
		tx("allowance", bob, tok, gcall("allowance(address,address)", addr(alice), addr(dave)))
		tx("nonces", bob, tok, gcall("nonces(address)", addr(permitOwner)))
		tx("hasRole", bob, tok, gcall("hasRole(bytes32,address)", h32(gkey("MINT_ROLE")), addr(alice)))
		tx("getRoleAdmin", bob, tok, gcall("getRoleAdmin(bytes32)", h32(gkey("MINT_ROLE"))))
		tx("policyId", bob, tok, gcall("policyId(bytes32)", h32(gkey("TRANSFER_SENDER_POLICY"))))
		tx("policyId bad", bob, tok, gcall("policyId(bytes32)", h32(gkey("NOPE"))))
		tx("isPaused", bob, tok, gcall("isPaused(uint8)", u(1)))
		tx("isPaused bad", bob, tok, gcall("isPaused(uint8)", u(9)))
	}
	// ERC-20 core
	tx("transfer", alice, A, gcall("transfer(address,uint256)", addr(bob), u(100)))
	tx("transfer again (warm)", alice, A, gcall("transfer(address,uint256)", addr(bob), u(100)))
	tx("transfer self", alice, A, gcall("transfer(address,uint256)", addr(alice), u(1)))
	tx("transfer zero", alice, A, gcall("transfer(address,uint256)", addr(carol), u(0)))
	tx("transfer to zero", alice, A, gcall("transfer(address,uint256)", addr(common.Address{}), u(1)))
	tx("transfer insufficient", bob, A, gcall("transfer(address,uint256)", addr(carol), u(1_000_000_000)))
	tx("transfer all of bob", bob, A, gcall("transfer(address,uint256)", addr(carol), u(200)))
	tx("approve", alice, A, gcall("approve(address,uint256)", addr(dave), u(1000)))
	tx("approve zero spender", alice, A, gcall("approve(address,uint256)", addr(common.Address{}), u(1)))
	tx("transferFrom", dave, A, gcall("transferFrom(address,address,uint256)", addr(alice), addr(carol), u(50)))
	tx("transferFrom too much", dave, A, gcall("transferFrom(address,address,uint256)", addr(alice), addr(carol), u(5000)))
	tx("approve max", alice, A, gcall("approve(address,uint256)", addr(dave), gstatic(gwBig(maxU))))
	tx("transferFrom infinite", dave, A, gcall("transferFrom(address,address,uint256)", addr(alice), addr(bob), u(7)))
	tx("approve clear", alice, A, gcall("approve(address,uint256)", addr(dave), u(0)))
	tx("transferFrom self no allowance", alice, A, gcall("transferFrom(address,address,uint256)", addr(alice), addr(bob), u(1)))
	// malformed
	tx("unknown selector", alice, A, gsel("nope()"))
	tx("short input", alice, A, []byte{0x01, 0x02})
	tx("empty input", alice, A, nil)
	tx("dirty address", alice, A, append(gsel("balanceOf(address)"), append([]byte{0x01}, make([]byte, 31)...)...))
	tx("truncated args", alice, A, gsel("transfer(address,uint256)"))
	record("payable", alice, A, gcall("transfer(address,uint256)", addr(bob), u(1)), 3_000_000, uint256.NewInt(1))
	tx("uninitialized token", alice, uninit, gcall("balanceOf(address)", addr(alice)))
	// policies
	tx("updatePolicy sender=blockBob", alice, A, gcall("updatePolicy(bytes32,uint64)", h32(gkey("TRANSFER_SENDER_POLICY")), u(blockBob)))
	tx("blocked sender", bob, A, gcall("transfer(address,uint256)", addr(carol), u(1)))
	tx("allowed sender", carol, A, gcall("transfer(address,uint256)", addr(bob), u(1)))
	tx("updatePolicy receiver=allow", alice, A, gcall("updatePolicy(bytes32,uint64)", h32(gkey("TRANSFER_RECEIVER_POLICY")), u(allowList)))
	tx("allowed receiver", alice, A, gcall("transfer(address,uint256)", addr(carol), u(3)))
	tx("forbidden receiver", alice, A, gcall("transfer(address,uint256)", addr(dave), u(3)))
	tx("updatePolicy executor=union", alice, A, gcall("updatePolicy(bytes32,uint64)", h32(gkey("TRANSFER_EXECUTOR_POLICY")), u(union)))
	tx("approve carol", alice, A, gcall("approve(address,uint256)", addr(carol), u(100)))
	tx("executor via union", carol, A, gcall("transferFrom(address,address,uint256)", addr(alice), addr(carol), u(5)))
	tx("approve dave again", alice, A, gcall("approve(address,uint256)", addr(dave), u(100)))
	tx("executor dave via union", dave, A, gcall("transferFrom(address,address,uint256)", addr(alice), addr(carol), u(5)))
	tx("updatePolicy unknown id", alice, A, gcall("updatePolicy(bytes32,uint64)", h32(gkey("MINT_RECEIVER_POLICY")), u(12345)))
	tx("updatePolicy bad scope", alice, A, gcall("updatePolicy(bytes32,uint64)", h32(gkey("NOPE")), u(0)))
	tx("updatePolicy not admin", bob, A, gcall("updatePolicy(bytes32,uint64)", h32(gkey("MINT_RECEIVER_POLICY")), u(0)))
	tx("updatePolicy mint=allow", alice, A, gcall("updatePolicy(bytes32,uint64)", h32(gkey("MINT_RECEIVER_POLICY")), u(allowList)))
	tx("mint forbidden receiver", alice, A, gcall("mint(address,uint256)", addr(dave), u(1)))
	tx("updatePolicy seize exempt=allow", alice, A, gcall("updatePolicy(bytes32,uint64)", h32(gkey("SEIZE_EXEMPT_POLICY")), u(allowList)))
	tx("policyId set", bob, A, gcall("policyId(bytes32)", h32(gkey("TRANSFER_EXECUTOR_POLICY"))))
	// pause
	tx("pause transfer", alice, A, gcall("pause(uint8[])", gwords(gw(0), gw(2))))
	tx("transfer paused", alice, A, gcall("transfer(address,uint256)", addr(carol), u(1)))
	tx("pausedFeatures", bob, A, gsel("pausedFeatures()"))
	tx("pause empty", alice, A, gcall("pause(uint8[])", gwords()))
	tx("pause bad feature", alice, A, gcall("pause(uint8[])", gwords(gw(7))))
	tx("pause not role", bob, A, gcall("pause(uint8[])", gwords(gw(1))))
	tx("unpause", alice, A, gcall("unpause(uint8[])", gwords(gw(0), gw(2))))
	// mint / burn / seize
	tx("mint", alice, A, gcall("mint(address,uint256)", addr(carol), u(500)))
	tx("mint not role", bob, A, gcall("mint(address,uint256)", addr(carol), u(500)))
	tx("mintWithMemo", alice, A, gcall("mintWithMemo(address,uint256,bytes32)", addr(carol), u(5), h32(gkey("memo"))))
	tx("burn", alice, A, gcall("burn(uint256)", u(10)))
	tx("burn too much", alice, A, gcall("burn(uint256)", u(1_000_000_000)))
	tx("burnWithMemo", alice, A, gcall("burnWithMemo(uint256,bytes32)", u(10), h32(gkey("memo2"))))
	tx("batchMint", alice, A, gcall("batchMint(address[],uint256[])", gwords(ga(alice), ga(carol)), gwords(gw(1), gw(2))))
	tx("batchMint mismatch", alice, A, gcall("batchMint(address[],uint256[])", gwords(ga(alice)), gwords(gw(1), gw(2))))
	tx("batchMint empty", alice, A, gcall("batchMint(address[],uint256[])", gwords(), gwords()))
	tx("updateSupplyCap below supply", alice, A, gcall("updateSupplyCap(uint256)", u(1)))
	tx("updateSupplyCap", alice, A, gcall("updateSupplyCap(uint256)", u(2_000_000)))
	tx("mint over cap", alice, A, gcall("mint(address,uint256)", addr(carol), u(2_000_000)))
	tx("seize exempt", alice, A, gcall("seizeWithMemo(address,address,uint256,bytes32)", addr(carol), addr(alice), u(1), h32(gkey("s"))))
	tx("seize self", alice, A, gcall("seizeWithMemo(address,address,uint256,bytes32)", addr(bob), addr(bob), u(1), h32(gkey("s"))))
	tx("seize", alice, A, gcall("seizeWithMemo(address,address,uint256,bytes32)", addr(bob), addr(carol), u(1), h32(gkey("s"))))
	// roles
	tx("grantRole", alice, A, gcall("grantRole(bytes32,address)", h32(gkey("MINT_ROLE")), addr(bob)))
	tx("grantRole again", alice, A, gcall("grantRole(bytes32,address)", h32(gkey("MINT_ROLE")), addr(bob)))
	tx("grantRole not admin", bob, A, gcall("grantRole(bytes32,address)", h32(gkey("MINT_ROLE")), addr(dave)))
	tx("renounceRole bad confirmation", bob, A, gcall("renounceRole(bytes32,address)", h32(gkey("MINT_ROLE")), addr(alice)))
	tx("renounceRole", bob, A, gcall("renounceRole(bytes32,address)", h32(gkey("MINT_ROLE")), addr(bob)))
	tx("revokeRole", alice, A, gcall("revokeRole(bytes32,address)", h32(gkey("BURN_ROLE")), addr(alice)))
	tx("revoke last admin", alice, A, gcall("revokeRole(bytes32,address)", h32(common.Hash{}), addr(alice)))
	tx("setRoleAdmin", alice, A, gcall("setRoleAdmin(bytes32,bytes32)", h32(gkey("BURN_ROLE")), h32(gkey("MINT_ROLE"))))
	tx("getRoleAdmin changed", bob, A, gcall("getRoleAdmin(bytes32)", h32(gkey("BURN_ROLE"))))
	tx("grant admin", alice, A, gcall("grantRole(bytes32,address)", h32(common.Hash{}), addr(dave)))
	tx("renounceLastAdmin not sole", alice, A, gsel("renounceLastAdmin()"))
	tx("renounceLastAdmin stranger", bob, A, gsel("renounceLastAdmin()"))
	tx("revoke dave admin", alice, A, gcall("revokeRole(bytes32,address)", h32(common.Hash{}), addr(dave)))
	// metadata (long strings exercise chunked storage, shrinking clears chunks)
	tx("updateName long", alice, A, gcall("updateName(string)", gstring("A considerably longer token name that spans several storage words")))
	tx("name long", bob, A, gsel("name()"))
	tx("updateName short", alice, A, gcall("updateName(string)", gstring("Short")))
	tx("updateSymbol", alice, A, gcall("updateSymbol(string)", gstring("CAT2")))
	tx("updateContractURI", alice, A, gcall("updateContractURI(string)", gstring("ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi")))
	tx("contractURI", bob, A, gsel("contractURI()"))
	tx("updateName not role", bob, A, gcall("updateName(string)", gstring("x")))
	tx("eip712Domain after rename", bob, A, gsel("eip712Domain()"))
	// memos
	tx("transferWithMemo", alice, A, gcall("transferWithMemo(address,uint256,bytes32)", addr(carol), u(2), h32(gkey("m1"))))
	tx("approve dave for memo", alice, A, gcall("approve(address,uint256)", addr(dave), u(100)))
	tx("transferFromWithMemo", dave, A, gcall("transferFromWithMemo(address,address,uint256,bytes32)", addr(alice), addr(carol), u(2), h32(gkey("m2"))))
	tx("transferWithMemoFormat", alice, A, gcall("transferWithMemoFormat(address,uint256,bytes32,bytes32)", addr(carol), u(2), h32(gkey("m3")), h32(gkey("fmt"))))
	tx("transferWithMemoFormat zero", alice, A, gcall("transferWithMemoFormat(address,uint256,bytes32,bytes32)", addr(carol), u(2), h32(gkey("m3")), h32(common.Hash{})))
	tx("transferFromWithMemoFormat", dave, A, gcall("transferFromWithMemoFormat(address,address,uint256,bytes32,bytes32)", addr(alice), addr(carol), u(2), h32(gkey("m4")), h32(gkey("fmt"))))
	// permit
	sign := func(k *ecdsa.PrivateKey, tok common.Address, name string, spender common.Address, value, nonce, deadline uint64) (byte, common.Hash, common.Hash) {
		dom := crypto.Keccak256(gh(gkey("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)")),
			crypto.Keccak256([]byte(name)), crypto.Keccak256([]byte("1")), gw(cfg.ChainID.Uint64()), ga(tok))
		st := crypto.Keccak256(gh(gkey("Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)")),
			ga(crypto.PubkeyToAddress(k.PublicKey)), ga(spender), gw(value), gw(nonce), gw(deadline))
		digest := crypto.Keccak256([]byte{0x19, 0x01}, dom, st)
		sig, _ := crypto.Sign(digest, k)
		return sig[64] + 27, common.BytesToHash(sig[:32]), common.BytesToHash(sig[32:64])
	}
	permit := func(name string, tok common.Address, tokName string, value, nonce, deadline uint64, tamper bool) {
		v, r, s := sign(key, tok, tokName, dave, value, nonce, deadline)
		if tamper {
			v ^= 1
		}
		tx(name, bob, tok, gcall("permit(address,address,uint256,uint256,uint8,bytes32,bytes32)",
			addr(permitOwner), addr(dave), u(value), u(deadline), u(uint64(v)), h32(r), h32(s)))
	}
	permit("permit", S, "CAS Stable Dollar Coin For The Golden Test", 77, 0, btime+60, false)
	permit("permit replay", S, "CAS Stable Dollar Coin For The Golden Test", 77, 0, btime+60, false)
	permit("permit wrong v", S, "CAS Stable Dollar Coin For The Golden Test", 77, 1, btime+60, true)
	permit("permit expired", S, "CAS Stable Dollar Coin For The Golden Test", 77, 1, btime-1, false)
	tx("permit bad v", bob, S, gcall("permit(address,address,uint256,uint256,uint8,bytes32,bytes32)",
		addr(permitOwner), addr(dave), u(1), u(btime+60), u(300), h32(common.Hash{}), h32(common.Hash{})))
	tx("allowance after permit", bob, S, gcall("allowance(address,address)", addr(permitOwner), addr(dave)))
	tx("stable transfer", alice, S, gcall("transfer(address,uint256)", addr(bob), u(9)))
	tx("stable currency", bob, S, gsel("currency()"))
	// asset extension
	tx("updateMultiplier", alice, A, gcall("updateMultiplier(uint256)", gstatic(gwBig(uint256.NewInt(2e18)))))
	tx("updateMultiplier zero", alice, A, gcall("updateMultiplier(uint256)", u(0)))
	tx("multiplier", bob, A, gsel("multiplier()"))
	tx("balanceOfUI", bob, A, gcall("balanceOfUI(address)", addr(alice)))
	tx("toUIAmount", bob, A, gcall("toUIAmount(uint256)", u(1234)))
	tx("fromUIAmount", bob, A, gcall("fromUIAmount(uint256)", u(1234)))
	tx("toUIAmount overflow", bob, A, gcall("toUIAmount(uint256)", gstatic(gwBig(maxU))))
	tx("totalSupplyUI", bob, A, gsel("totalSupplyUI()"))
	tx("updateUIMultiplier", alice, A, gcall("updateUIMultiplier(uint256,uint256)", gstatic(gwBig(uint256.NewInt(3e18))), u(btime+100)))
	tx("newUIMultiplier", bob, A, gsel("newUIMultiplier()"))
	tx("effectiveAt", bob, A, gsel("effectiveAt()"))
	tx("updateUIMultiplier exists", alice, A, gcall("updateUIMultiplier(uint256,uint256)", gstatic(gwBig(uint256.NewInt(4e18))), u(btime+200)))
	tx("updateUIMultiplier past", alice, A, gcall("updateUIMultiplier(uint256,uint256)", gstatic(gwBig(uint256.NewInt(4e18))), u(btime)))
	tx("cancelUIMultiplierUpdate", alice, A, gsel("cancelUIMultiplierUpdate()"))
	tx("cancel again", alice, A, gsel("cancelUIMultiplierUpdate()"))
	tx("supportsInterface", bob, A, gcall("supportsInterface(bytes4)", gstatic(append([]byte{0xa6, 0x0b, 0xf1, 0x3d}, make([]byte, 28)...))))
	tx("supportsInterface dirty", bob, A, gcall("supportsInterface(bytes4)", u(1)))
	tx("updateExtraMetadata", alice, A, gcall("updateExtraMetadata(string,string)", gstring("isin"), gstring("US0000000000 with a value longer than one storage word")))
	tx("extraMetadata", bob, A, gcall("extraMetadata(string)", gstring("isin")))
	tx("updateExtraMetadata empty key", alice, A, gcall("updateExtraMetadata(string,string)", gstring(""), gstring("v")))
	inner1 := gcall("updateMultiplier(uint256)", gstatic(gwBig(uint256.NewInt(5e17))))
	inner2 := gcall("updateExtraMetadata(string,string)", gstring("k"), gstring("v"))
	tx("announce", alice, A, gcall("announce(bytes[],string,string,string)", gbytesArray(inner1, inner2), gstring("ann-1"), gstring("split"), gstring("ipfs://x")))
	tx("announce reused id", alice, A, gcall("announce(bytes[],string,string,string)", gbytesArray(), gstring("ann-1"), gstring("d"), gstring("u")))
	nested := gcall("announce(bytes[],string,string,string)", gbytesArray(), gstring("ann-3"), gstring("d"), gstring("u"))
	tx("announce nested", alice, A, gcall("announce(bytes[],string,string,string)", gbytesArray(nested), gstring("ann-2"), gstring("d"), gstring("u")))
	tx("announce malformed call", alice, A, gcall("announce(bytes[],string,string,string)", gbytesArray([]byte{1}), gstring("ann-4"), gstring("d"), gstring("u")))
	tx("isAnnouncementIdUsed", bob, A, gcall("isAnnouncementIdUsed(string)", gstring("ann-1")))
	// through the proxy: call forms, static frames and precise gas limits
	pcall := func(name string, op byte, target common.Address, gas uint64, payload []byte) {
		tx(name, alice, proxy, proxyInput(op, target, gas, payload))
	}
	bal := gcall("balanceOf(address)", addr(alice))
	xfer := gcall("transfer(address,uint256)", addr(carol), u(1))
	pcall("static view", 1, A, 1_000_000, bal)
	pcall("static write", 1, A, 1_000_000, xfer)
	pcall("delegatecall", 2, A, 1_000_000, bal)
	pcall("callcode", 3, A, 1_000_000, bal)
	pcall("call transfer from proxy (no balance)", 0, A, 1_000_000, xfer)
	pcall("static policy-gated approve", 1, A, 1_000_000, gcall("approve(address,uint256)", addr(dave), u(1)))
	pcall("milliTimestamp", 1, common.BytesToAddress([]byte{0x70}), 100_000, nil)
	pcall("milliTimestamp oog", 1, common.BytesToAddress([]byte{0x70}), 19, nil)
	pcall("double sign garbage", 0, common.BytesToAddress([]byte{0x68}), 200_000, make([]byte, 100))
	// Gas sweeps: every limit from far below to just above what the call needs.
	tx("fund proxy", alice, S, gcall("transfer(address,uint256)", addr(proxy), u(1000)))
	for gas := uint64(0); gas <= 40_000; gas += 211 {
		pcall("sweep transfer", 0, S, gas, xfer)
	}
	// A zero-amount self transfer writes both balances back unchanged: each write is a
	// 100-gas no-op that only the EIP-2200 sentry (remaining gas must exceed 2300) refuses.
	// The reads before the first write cost about 6,460 gas, so the sentry decides the
	// outcome for limits between roughly 6,560 and 8,760.
	for gas := uint64(6_300); gas <= 11_500; gas += 41 {
		pcall("sweep sentry", 0, S, gas, gcall("transfer(address,uint256)", addr(proxy), u(0)))
	}
	for gas := uint64(0); gas <= 12_000; gas += 307 {
		pcall("sweep static name", 1, S, gas, gsel("name()"))
	}
	for gas := uint64(0); gas <= 12_000; gas += 499 {
		pcall("sweep static write", 1, S, gas, xfer)
	}
	for gas := uint64(0); gas <= 60_000; gas += 1_499 {
		pcall("sweep policy-gated transfer", 0, A, gas, gcall("transfer(address,uint256)", addr(carol), u(1)))
	}

	// --- post-state -----------------------------------------------------------------
	for k := range rec.slots {
		if _, ok := g.Post[k.addr]; !ok {
			g.Post[k.addr] = map[common.Hash]common.Hash{}
		}
		g.Post[k.addr][k.slot] = statedb.GetState(k.addr, k.slot)
	}
	_ = sort.Strings
	blob, _ := json.MarshalIndent(g, "", " ")
	if err := os.WriteFile(out, blob, 0o644); err != nil {
		t.Fatal(err)
	}
	t.Logf("wrote %d txs", len(g.Txs))
}
