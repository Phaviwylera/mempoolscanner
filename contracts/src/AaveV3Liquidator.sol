// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {IFlashLoanSimpleReceiver} from "./interfaces/IFlashLoanSimpleReceiver.sol";
import {IPool} from "./interfaces/IPool.sol";
import {IPoolAddressesProvider} from "./interfaces/IPoolAddressesProvider.sol";
import {ISwapRouter02} from "./interfaces/ISwapRouter02.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";
import {Ownable2Step} from "@openzeppelin/contracts/access/Ownable2Step.sol";
import {ReentrancyGuard} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";

/**
 * @title AaveV3Liquidator
 *
 * Aave V3 liquidation receiver. Funds a liquidation with an Aave flash loan,
 * seizes the borrower's collateral, swaps it back to the debt asset through
 * Uniswap V3, repays the loan, and forwards the remainder to the owner.
 *
 * The contract never needs capital of its own: everything happens inside one
 * atomic `flashLoanSimple` callback, and if any leg fails the whole thing
 * reverts.
 *
 * Access control is split: `owner` (Ownable2Step) manages configuration and
 * receives profit; `executor` is the hot key allowed to trigger liquidations.
 */
contract AaveV3Liquidator is IFlashLoanSimpleReceiver, Ownable2Step, ReentrancyGuard {
    using SafeERC20 for IERC20;

    IPool public immutable POOL;
    ISwapRouter02 public immutable SWAP_ROUTER;

    address public executor;

    /// @notice Owner-managed allowlist of external swap routers the generic
    /// collateral-exit path may call (Curve pools, 1inch/0x/Odos routers, …).
    /// The exit calls the router with executor-supplied calldata, so restricting
    /// it to vetted routers stops a leaked executor key from pointing the seized
    /// collateral at a malicious contract mid-liquidation.
    mapping(address => bool) public allowedSwapRouter;

    /// @dev Flash-loan params. Decoded as a single memory pointer: the separate
    /// locals plus the callback's own arguments overflow the stack otherwise.
    struct LiqParams {
        address user;
        address collateralAsset;
        address debtAsset;
        uint24 swapFee;
        uint256 minProfit;
        // Generic collateral-exit: when `swapCalldata` is non-empty the seized
        // collateral is converted to the debt asset by calling `swapRouter` with
        // `swapCalldata` (built off-chain); otherwise the Uniswap V3 single-hop
        // (`SWAP_ROUTER`, `swapFee`) is used, exactly as before.
        address swapRouter;
        bytes swapCalldata;
    }

    event LiquidationExecuted(
        address indexed user,
        address indexed debtAsset,
        uint256 debtRepaid,
        uint256 collateralSeized,
        uint256 profit
    );

    event TokenRescued(address indexed token, uint256 amount, address indexed to);
    event ExecutorUpdated(address indexed oldExecutor, address indexed newExecutor);
    event SwapRouterAllowed(address indexed router, bool allowed);

    modifier onlyExecutor() {
        require(msg.sender == executor, "NOT_EXECUTOR");
        _;
    }

    constructor(
        address poolAddressesProvider,
        address swapRouter,
        address initialOwner
    ) Ownable(initialOwner) {
        require(poolAddressesProvider != address(0), "ZERO_PROVIDER");
        require(swapRouter != address(0), "ZERO_ROUTER");
        require(initialOwner != address(0), "ZERO_OWNER");

        POOL = IPool(IPoolAddressesProvider(poolAddressesProvider).getPool());
        SWAP_ROUTER = ISwapRouter02(swapRouter);
        // Ownable(initialOwner) already set the owner.
        executor = initialOwner;
    }

    /**
     * @notice Trigger a flash-loan funded liquidation.
     * @param user          borrower whose position is underwater
     * @param collateralAsset asset to seize
     * @param debtAsset     asset to repay (also the flash-loaned asset)
     * @param debtToCover   amount of debt to repay, in debtAsset units
     * @param swapFee       Uniswap V3 fee tier for collateral -> debt (used only
     *                      when swapCalldata is empty)
     * @param minProfit     revert unless at least this much debtAsset is left
     *                      over after repaying the loan
     * @param swapRouter    external router for the generic exit (address(0) +
     *                      empty swapCalldata selects the Uniswap V3 path)
     * @param swapCalldata  pre-built collateral->debt swap calldata for
     *                      swapRouter; empty selects the Uniswap V3 path
     */
    function executeLiquidation(
        address user,
        address collateralAsset,
        address debtAsset,
        uint256 debtToCover,
        uint24 swapFee,
        uint256 minProfit,
        address swapRouter,
        bytes calldata swapCalldata
    ) external onlyExecutor nonReentrant {
        require(debtToCover > 0, "ZERO_DEBT");

        bytes memory params = abi.encode(
            LiqParams({
                user: user,
                collateralAsset: collateralAsset,
                debtAsset: debtAsset,
                swapFee: swapFee,
                minProfit: minProfit,
                swapRouter: swapRouter,
                swapCalldata: swapCalldata
            })
        );
        POOL.flashLoanSimple(address(this), debtAsset, debtToCover, params, 0);
    }

    /**
     * @dev Aave flash-loan callback. The liquidation, swap, repayment and
     * profit sweep all happen here, atomically.
     *
     * Split across helper functions deliberately: holding every intermediate
     * value in one frame overflows the EVM stack ("stack too deep") without
     * via-ir, and via-ir would slow every compile for no runtime benefit.
     */
    function executeOperation(
        address asset,
        uint256 amount,
        uint256 premium,
        address initiator,
        bytes calldata params
    ) external override returns (bool) {
        require(msg.sender == address(POOL), "CALLER_NOT_POOL");
        require(initiator == address(this), "INITIATOR_NOT_SELF");

        LiqParams memory q = abi.decode(params, (LiqParams));

        // The flash-loaned asset must be the debt we intend to repay.
        require(asset == q.debtAsset, "ASSET_MISMATCH");

        uint256 amountOwed = amount + premium;
        uint256 seized = _liquidate(q.collateralAsset, q.debtAsset, q.user, amount);

        if (q.swapCalldata.length > 0) {
            // Generic exit: convert collateral -> debt through a vetted external
            // router (collateral with no deep Uniswap V3 pool — e.g. sUSDe and
            // other vault tokens — routed via Curve or an aggregator). The router
            // sets its own minOut, so enforce the profit floor here on the
            // post-swap balance; a bad or thin swap then reverts the bundle.
            _swapViaRouter(q.collateralAsset, q.swapRouter, q.swapCalldata, seized);
            require(
                IERC20(asset).balanceOf(address(this)) >= amountOwed + q.minProfit,
                "BELOW_MIN_PROFIT"
            );
        } else {
            // amountOutMinimum doubles as the profitability gate: if the swap
            // cannot clear the loan plus minProfit, the whole bundle reverts.
            _swapExactIn(q.collateralAsset, asset, q.swapFee, seized, amountOwed + q.minProfit);
        }

        uint256 balance = IERC20(asset).balanceOf(address(this));
        require(balance >= amountOwed, "CANNOT_REPAY");

        // The pool pulls amountOwed from us after this returns.
        IERC20(asset).forceApprove(address(POOL), amountOwed);

        // Profit is only what sits ABOVE the repayment. Sweeping the whole
        // balance would leave the pool nothing to pull and revert everything.
        uint256 profit = balance - amountOwed;
        if (profit > 0) {
            IERC20(asset).safeTransfer(owner(), profit);
        }

        emit LiquidationExecuted(q.user, q.debtAsset, amount, seized, profit);
        return true;
    }

    /// @dev Liquidate and return how much collateral actually arrived.
    function _liquidate(
        address collateralAsset,
        address debtAsset,
        address user,
        uint256 debtToCover
    ) private returns (uint256 seized) {
        uint256 balanceBefore = IERC20(collateralAsset).balanceOf(address(this));

        // liquidationCall pulls debtToCover of the debt asset FROM this
        // contract, so the pool needs an allowance before the call -- not just
        // the later one for the flash-loan repayment. Without this the call
        // reverts with "ERC20: transfer amount exceeds allowance".
        IERC20(debtAsset).forceApprove(address(POOL), debtToCover);

        // receiveAToken=false: take the underlying, which is what gets swapped.
        POOL.liquidationCall(collateralAsset, debtAsset, user, debtToCover, false);

        // Guard BEFORE subtracting: if the liquidation seized nothing (e.g. the
        // pool capped it to zero), `after - before` would underflow and panic
        // inside the flash-loan callback instead of giving a clean revert, and
        // the contract would otherwise go on to swap 0 tokens on Uniswap.
        uint256 collateralAfter = IERC20(collateralAsset).balanceOf(address(this));
        require(collateralAfter > balanceBefore, "NOTHING_SEIZED");
        seized = collateralAfter - balanceBefore;
    }

    /// @dev Single-hop exact-input swap on Uniswap V3.
    function _swapExactIn(
        address tokenIn,
        address tokenOut,
        uint24 fee,
        uint256 amountIn,
        uint256 minOut
    ) private {
        IERC20(tokenIn).forceApprove(address(SWAP_ROUTER), amountIn);

        SWAP_ROUTER.exactInputSingle(
            ISwapRouter02.ExactInputSingleParams({
                tokenIn: tokenIn,
                tokenOut: tokenOut,
                fee: fee,
                recipient: address(this),
                amountIn: amountIn,
                amountOutMinimum: minOut,
                sqrtPriceLimitX96: 0
            })
        );
    }

    /// @dev Generic collateral-exit: approve the seized collateral to a vetted
    /// router and run executor-supplied swap calldata (Curve/1inch/0x/Odos/…).
    /// The resulting debt asset lands in this contract; the caller enforces the
    /// profit floor afterwards. The off-chain calldata must swap an amount <= the
    /// seized collateral; any dust left behind is recoverable via rescueTokens.
    function _swapViaRouter(
        address tokenIn,
        address router,
        bytes memory data,
        uint256 amountIn
    ) private {
        require(allowedSwapRouter[router], "ROUTER_NOT_ALLOWED");
        IERC20(tokenIn).forceApprove(router, amountIn);
        (bool ok, ) = router.call(data);
        require(ok, "ROUTER_SWAP_FAILED");
        // Clear any residual allowance the router did not consume.
        IERC20(tokenIn).forceApprove(router, 0);
    }

    /// @notice Allow or disallow an external swap router for the generic exit
    /// path. Owner only. Add vetted routers (e.g. a Curve pool) before using a
    /// non-empty swapCalldata that targets them.
    function setSwapRouter(address router, bool allowed) external onlyOwner {
        require(router != address(0), "ZERO_ROUTER");
        allowedSwapRouter[router] = allowed;
        emit SwapRouterAllowed(router, allowed);
    }

    /// @notice Rotate the hot executor key. Owner only.
    function setExecutor(address newExecutor) external onlyOwner {
        require(newExecutor != address(0), "ZERO_EXECUTOR");
        address old = executor;
        executor = newExecutor;
        emit ExecutorUpdated(old, newExecutor);
    }

    /// @notice Recover tokens sent here by accident. Owner only.
    function rescueTokens(address token, uint256 amount, address to) external onlyOwner nonReentrant {
        require(to != address(0), "ZERO_RECIPIENT");
        IERC20(token).safeTransfer(to, amount);
        emit TokenRescued(token, amount, to);
    }
}
