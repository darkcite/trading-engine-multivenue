// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
pragma solidity 0.8.28;

import {HyparbExecutor} from "../src/HyparbExecutor.sol";

/// Minimal ERC-20. `mode` 0: returns true; 1: returns nothing (USDT
/// style); 2: returns false.
contract MockToken {
    mapping(address => uint256) public balanceOf;
    uint8 public mode;

    function setMode(uint8 m) external {
        mode = m;
    }

    function mint(address to, uint256 a) external {
        balanceOf[to] += a;
    }

    function transfer(address to, uint256 a) external {
        if (mode == 2) {
            assembly {
                mstore(0, 0)
                return(0, 32)
            }
        }
        require(balanceOf[msg.sender] >= a, "balance");
        balanceOf[msg.sender] -= a;
        balanceOf[to] += a;
        if (mode == 0) {
            assembly {
                mstore(0, 1)
                return(0, 32)
            }
        }
    }
}

interface IV3Callback {
    function uniswapV3SwapCallback(int256, int256, bytes calldata) external;
}

interface IAlgebraCallback {
    function algebraSwapCallback(int256, int256, bytes calldata) external;
}

interface IHyperswapCallback {
    function hyperswapV3SwapCallback(int256, int256, bytes calldata) external;
}

// The callback name a pool family calls.
uint8 constant V3 = 0;
uint8 constant ALGEBRA = 1;
uint8 constant HYPERSWAP = 2;

/// Call `exec`'s callback under `family`'s name.
function callBack(address exec, uint8 family, int256 a0, int256 a1, bytes memory data) {
    if (family == ALGEBRA) {
        IAlgebraCallback(exec).algebraSwapCallback(a0, a1, data);
    } else if (family == HYPERSWAP) {
        IHyperswapCallback(exec).hyperswapV3SwapCallback(a0, a1, data);
    } else {
        IV3Callback(exec).uniswapV3SwapCallback(a0, a1, data);
    }
}

/// A pool that behaves like the contracts: pay out first, call back,
/// then insist its input balance grew. Price: 1 token0 = 2 token1.
/// `family` selects the callback name; an impostor makes it call back
/// into the executor from a DIFFERENT contract instead.
contract MockPool {
    MockToken public immutable token0;
    MockToken public immutable token1;
    uint8 public immutable family;
    Impostor public impostor;

    constructor(MockToken t0, MockToken t1, uint8 fam) {
        token0 = t0;
        token1 = t1;
        family = fam;
    }

    function setImpostor(Impostor i) external {
        impostor = i;
    }

    function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160, bytes calldata data)
        external
        returns (int256 amount0, int256 amount1)
    {
        require(amountSpecified > 0, "exact-in only in the mock");
        uint256 inAmt = uint256(amountSpecified);
        uint256 outAmt = zeroForOne ? inAmt * 2 : inAmt / 2;
        (amount0, amount1) = zeroForOne ? (int256(inAmt), -int256(outAmt)) : (-int256(outAmt), int256(inAmt));
        MockToken tin = zeroForOne ? token0 : token1;
        MockToken tout = zeroForOne ? token1 : token0;
        tout.transfer(recipient, outAmt);
        uint256 before = tin.balanceOf(address(this));
        if (address(impostor) != address(0)) {
            impostor.poke(msg.sender, amount0, amount1, family);
        } else {
            callBack(msg.sender, family, amount0, amount1, data);
        }
        require(tin.balanceOf(address(this)) >= before + inAmt, "IIA");
    }
}

/// Calls the executor's callback while pretending to be owed.
contract Impostor {
    function poke(address exec, int256 a0, int256 a1, uint8 family) external {
        callBack(exec, family, a0, a1, "");
    }
}

/// Anyone who is not the owner.
contract Stranger {
    function trySwap(HyparbExecutor x, address pool) external returns (bool ok) {
        (ok,) = address(x).call(abi.encodeCall(HyparbExecutor.swap, (pool, true, 1, 0, 0)));
    }

    function trySweep(HyparbExecutor x, address token) external returns (bool ok) {
        (ok,) = address(x).call(abi.encodeCall(HyparbExecutor.sweep, (token, address(this), 1)));
    }
}

contract HyparbExecutorTest {
    HyparbExecutor x;
    MockToken t0;
    MockToken t1;
    MockPool v3;
    MockPool alg;
    MockPool hs;

    function setUp() public {
        x = new HyparbExecutor();
        t0 = new MockToken();
        t1 = new MockToken();
        v3 = new MockPool(t0, t1, V3);
        alg = new MockPool(t0, t1, ALGEBRA);
        hs = new MockPool(t0, t1, HYPERSWAP);
        t0.mint(address(x), 1_000);
        t1.mint(address(x), 1_000);
        t0.mint(address(v3), 1_000_000);
        t1.mint(address(v3), 1_000_000);
        t0.mint(address(alg), 1_000_000);
        t1.mint(address(alg), 1_000_000);
        t0.mint(address(hs), 1_000_000);
        t1.mint(address(hs), 1_000_000);
    }

    function _revertSelector(bytes memory ret) private pure returns (bytes4 s) {
        assembly {
            s := mload(add(ret, 32))
        }
    }

    function test_owner_is_the_deployer() public view {
        require(x.owner() == address(this), "owner");
    }

    function test_v3_swap_pays_in_the_callback_and_keeps_the_output() public {
        (int256 a0, int256 a1) = x.swap(address(v3), true, 100, 0, 200);
        require(a0 == 100 && a1 == -200, "deltas");
        require(t0.balanceOf(address(x)) == 900 && t1.balanceOf(address(x)) == 1_200, "inventory");
    }

    function test_algebra_swap_uses_its_own_callback() public {
        (int256 a0, int256 a1) = x.swap(address(alg), false, 100, 0, 50);
        require(a0 == -50 && a1 == 100, "deltas");
        require(t0.balanceOf(address(x)) == 1_050 && t1.balanceOf(address(x)) == 900, "inventory");
    }

    function test_hyperswap_swap_uses_its_own_callback() public {
        // The pool the H8 battery's first swap reverted on (2026-09-24):
        // a Hyperswap V3 pool calls `hyperswapV3SwapCallback`, which the
        // first executor did not answer.
        (int256 a0, int256 a1) = x.swap(address(hs), true, 100, 0, 200);
        require(a0 == 100 && a1 == -200, "deltas");
        require(t0.balanceOf(address(x)) == 900 && t1.balanceOf(address(x)) == 1_200, "inventory");
    }

    function test_below_min_out_reverts_and_moves_nothing() public {
        (bool ok, bytes memory ret) = address(x).call(abi.encodeCall(HyparbExecutor.swap, (address(v3), true, 100, 0, 201)));
        require(!ok, "must revert");
        require(_revertSelector(ret) == HyparbExecutor.BelowMinOut.selector, "BelowMinOut");
        require(t0.balanceOf(address(x)) == 1_000 && t1.balanceOf(address(x)) == 1_000, "untouched");
    }

    function test_only_the_owner_swaps_or_sweeps() public {
        Stranger s = new Stranger();
        require(!s.trySwap(x, address(v3)), "stranger swapped");
        require(!s.trySweep(x, address(t0)), "stranger swept");
        x.sweep(address(t0), address(0xbeef), 10);
        require(t0.balanceOf(address(0xbeef)) == 10, "owner sweep");
    }

    function test_a_callback_outside_a_swap_is_refused() public {
        (bool ok, bytes memory ret) = address(x).call(abi.encodeCall(HyparbExecutor.uniswapV3SwapCallback, (int256(1), int256(0), bytes(""))));
        require(!ok && _revertSelector(ret) == HyparbExecutor.NotPool.selector, "NotPool outside");
        (ok, ret) = address(x).call(abi.encodeCall(HyparbExecutor.algebraSwapCallback, (int256(1), int256(0), bytes(""))));
        require(!ok && _revertSelector(ret) == HyparbExecutor.NotPool.selector, "NotPool outside (algebra)");
        (ok, ret) = address(x).call(abi.encodeCall(HyparbExecutor.hyperswapV3SwapCallback, (int256(1), int256(0), bytes(""))));
        require(!ok && _revertSelector(ret) == HyparbExecutor.NotPool.selector, "NotPool outside (hyperswap)");
    }

    function test_a_callback_from_another_address_during_a_swap_is_refused() public {
        MockPool[3] memory pools = [v3, alg, hs];
        for (uint256 i = 0; i < 3; i++) {
            pools[i].setImpostor(new Impostor());
            (bool ok,) = address(x).call(abi.encodeCall(HyparbExecutor.swap, (address(pools[i]), true, 100, 0, 0)));
            require(!ok, "an impostor was paid");
        }
        require(t0.balanceOf(address(x)) == 1_000, "nothing paid");
    }

    function test_the_callback_selectors_are_the_families() public pure {
        // Measured in the pools' bytecode: Uniswap V3 / Slipstream,
        // Algebra Integral, Hyperswap V3 (mainnet factory 0xb1c0…02e3).
        require(HyparbExecutor.uniswapV3SwapCallback.selector == bytes4(0xfa461e33), "v3");
        require(HyparbExecutor.algebraSwapCallback.selector == bytes4(0x2c8958f6), "algebra");
        require(HyparbExecutor.hyperswapV3SwapCallback.selector == bytes4(0xfa85398b), "hyperswap");
    }

    function test_usdt_style_tokens_pay_and_false_returning_tokens_revert() public {
        t0.setMode(1);
        x.swap(address(v3), true, 100, 0, 200);
        require(t0.balanceOf(address(v3)) == 1_000_100, "paid without a return value");
        t0.setMode(2);
        (bool ok,) = address(x).call(abi.encodeCall(HyparbExecutor.swap, (address(v3), true, 100, 0, 0)));
        require(!ok, "a false transfer must revert");
    }

    function test_nothing_owed_is_refused() public {
        // A pool that reports no positive delta: refuse rather than guess.
        NothingPool np = new NothingPool();
        (bool ok, bytes memory ret) = address(x).call(abi.encodeCall(HyparbExecutor.swap, (address(np), true, 1, 0, 0)));
        require(!ok, "must revert");
        require(_revertSelector(ret) == HyparbExecutor.NothingOwed.selector, "NothingOwed");
    }
}

contract NothingPool {
    function swap(address, bool, int256, uint160, bytes calldata) external returns (int256, int256) {
        IV3Callback(msg.sender).uniswapV3SwapCallback(0, 0, "");
        return (0, 0);
    }
}
