// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
pragma solidity 0.8.28;

/// Every concentrated-liquidity family HYPARB trades shares this entry
/// point: Uniswap V3 and its forks (Slipstream / Hybra CL) and Algebra
/// Integral all expose `swap(address,bool,int256,uint160,bytes)`
/// (selector 0x128acb08) and immutable `token0()` / `token1()`.
interface IClPool {
    function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160 sqrtPriceLimitX96, bytes calldata data)
        external
        returns (int256 amount0, int256 amount1);

    function token0() external view returns (address);

    function token1() external view returns (address);
}

/// @title HYPARB executor (O-H18)
/// @notice One swap per call against a pool the OWNER names, paid from
/// this contract's own balance inside the pool's callback, and reverted
/// unless at least `minOut` of the output token arrived. An unprofitable
/// race costs gas, never inventory. No routers, no approvals, no upgrade
/// path, no admin beyond the deployer.
/// @dev An EOA cannot swap against a pool directly: the pool pays out,
/// then calls back into `msg.sender` to collect the input and checks its
/// own balance. This contract is that `msg.sender`.
///
/// The pool being swapped is held in TRANSIENT storage (EIP-1153 —
/// HyperEVM runs Cancun) for the duration of the call only: a callback
/// from any other address, or from any address outside a swap, reverts.
/// Both callback names route to the same payment:
/// `uniswapV3SwapCallback` (V3, Slipstream) and `algebraSwapCallback`
/// (Algebra Integral, O-H19).
contract HyparbExecutor {
    /// The deployer: the only address that may swap or sweep.
    address public immutable owner;

    /// keccak256("hyparb.executor.pool") - 1
    bytes32 private constant POOL_SLOT = 0x89591f059b966fa4f4976195994c15a214b56effd20a07787a56d4d2fafcb820;

    error NotOwner();
    error NotPool();
    error NothingOwed();
    error BelowMinOut(uint256 received, uint256 minOut);
    error TransferFailed();

    constructor() {
        owner = msg.sender;
    }

    /// @notice Swap against `pool`; the output stays in this contract.
    /// @param amountSpecified > 0 exact input, < 0 exact output (the pool's convention).
    /// @param minOut Revert unless at least this much of the output token arrived.
    function swap(address pool, bool zeroForOne, int256 amountSpecified, uint160 sqrtPriceLimitX96, uint256 minOut)
        external
        returns (int256 amount0, int256 amount1)
    {
        if (msg.sender != owner) revert NotOwner();
        assembly ("memory-safe") {
            tstore(POOL_SLOT, pool)
        }
        (amount0, amount1) = IClPool(pool).swap(address(this), zeroForOne, amountSpecified, sqrtPriceLimitX96, "");
        assembly ("memory-safe") {
            tstore(POOL_SLOT, 0)
        }
        int256 outDelta = zeroForOne ? amount1 : amount0;
        uint256 received = outDelta < 0 ? uint256(-outDelta) : 0;
        if (received < minOut) revert BelowMinOut(received, minOut);
    }

    /// @notice V3 / Slipstream swap callback.
    function uniswapV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        _pay(amount0Delta, amount1Delta);
    }

    /// @notice Algebra Integral swap callback.
    function algebraSwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        _pay(amount0Delta, amount1Delta);
    }

    /// @notice Move inventory out (funding unwinds). Owner only.
    function sweep(address token, address to, uint256 amount) external {
        if (msg.sender != owner) revert NotOwner();
        _transfer(token, to, amount);
    }

    /// Pay the pool what it is owed — only the pool this call is
    /// swapping, only inside the swap.
    function _pay(int256 amount0Delta, int256 amount1Delta) private {
        address pool;
        assembly ("memory-safe") {
            pool := tload(POOL_SLOT)
        }
        if (pool == address(0) || msg.sender != pool) revert NotPool();
        if (amount0Delta > 0) {
            _transfer(IClPool(pool).token0(), pool, uint256(amount0Delta));
        } else if (amount1Delta > 0) {
            _transfer(IClPool(pool).token1(), pool, uint256(amount1Delta));
        } else {
            revert NothingOwed();
        }
    }

    /// ERC-20 `transfer` that accepts tokens returning nothing (USDT
    /// style) and refuses a `false` return, a revert, or an address with
    /// no code (whose call would "succeed" and move nothing).
    function _transfer(address token, address to, uint256 amount) private {
        if (token.code.length == 0) revert TransferFailed();
        (bool ok, bytes memory ret) = token.call(abi.encodeWithSelector(0xa9059cbb, to, amount));
        if (!ok || (ret.length != 0 && (ret.length != 32 || abi.decode(ret, (uint256)) != 1))) revert TransferFailed();
    }
}
