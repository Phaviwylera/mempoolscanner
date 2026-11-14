// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {IFlashLoanSimpleReceiver} from "./interfaces/IFlashLoanSimpleReceiver.sol";
import {IPool} from "./interfaces/IPool.sol";
import {IPoolAddressesProvider} from "./interfaces/IPoolAddressesProvider.sol";
import {ISwapRouter02} from "./interfaces/ISwapRouter02.sol";
import {IERC20} from "./interfaces/IERC20.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";
import {Ownable2Step} from "@openzeppelin/contracts/access/Ownable2Step.sol";
import {ReentrancyGuard} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";

/**
 * @title AaveV3Liquidator
 * @author MempoolScanner
 *
 * Aave V3 liquidation receiver contract. Executes liquidations via flash loans
 * and repays the flash loan by swapping seized collateral through Uniswap V3.
 *
 * Access control: only the configured `executor` can call `executeLiquidation`.
 * Ownership (Ownable2Step) is separate from the executor role.
 */
contract AaveV3Liquidator is IFlashLoanSimpleReceiver, Ownable2Step, ReentrancyGuard {
    IPool public immutable POOL;
    ISwapRouter02 public immutable SWAP_ROUTER;

    address public executor;

    event LiquidationExecuted(
        address indexed user,
        address indexed debtAsset,
        uint256 debtRepaid,
        uint256 collateralSeized,
        uint256 profit
    );

    event TokenRescued(address indexed token, uint256 amount, address indexed to);

    event ExecutorUpdated(address indexed oldExecutor, address indexed newExecutor);

    modifier onlyExecutor() {
        require(msg.sender == executor, "NOT_EXECUTOR");
        _;
    }

    constructor(
        address poolAddressesProvider,
        address swapRouter,
        address initialOwner
    ) Ownable2Step(initialOwner) {
        require(poolAddressesProvider != address(0), "ZERO_PROVIDER");
        require(swapRouter != address(0), "ZERO_ROUTER");
        require(initialOwner != address(0), "ZERO_OWNER");

        POOL = IPool(IPoolAddressesProvider(poolAddressesProvider).getPool());
        SWAP_ROUTER = ISwapRouter02(swapRouter);
        _transferOwnership(initialOwner);
        executor = initialOwner;
    }

    /**
     * @dev Entry point for liquidation. Only callable by the executor.
     * Initiates an Aave V3 flash loan to fund the liquidation.
     */
    function executeLiquidation(
        address user,
        address collateralAsset,
        address debtAsset,
        uint256 debtToCover,
        uint24 swapFee,
        uint256 minProfit
    ) external onlyExecutor nonReentrant {
        require(debtToCover > 0, "ZERO_DEBT");

        bytes memory params = abi.encode(user, collateralAsset, debtAsset, swapFee, minProfit);

        POOL.flashLoanSimple(address(this), debtAsset, debtToCover, params, 0);
    }

    /**
     * @dev Flash loan callback from Aave V3 Pool. Only callable by the pool.
     * Executes the liquidation, swaps seized collateral to repay the flash loan,
     * and transfers any profit to the owner.
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

        (address user, address collateralAsset, address debtAsset, uint24 swapFee, uint256 minProfit)
            = abi.decode(params, (address, address, address, uint24, uint256));

        uint256 collateralBefore = IERC20(collateralAsset).balanceOf(address(this));

        POOL.liquidationCall(collateralAsset, debtAsset, user, amount, false);

        uint256 collateralAfter = IERC20(collateralAsset).balanceOf(address(this));
        uint256 seized = collateralAfter - collateralBefore;
        require(seized > 0, "NOTHING_SEIZED");

        IERC20(collateralAsset).approve(address(SWAP_ROUTER), seized);

        uint256 amountOwed = amount + premium;
        uint256 minOut = amountOwed + minProfit;

        ISwapRouter02.ExactInputSingleParams memory p = ISwapRouter02.ExactInputSingleParams({
            tokenIn:          collateralAsset,
            tokenOut:         asset,
            fee:              swapFee,
            recipient:        address(this),
            amountIn:         seized,
            amountOutMinimum: minOut,
            sqrtPriceLimitX96: 0
        });

        SWAP_ROUTER.exactInputSingle(p);

        require(IERC20(asset).balanceOf(address(this)) >= amountOwed, "CANNOT_REPAY");

        // Approve the pool to pull the flash-loan repayment (amount + premium).
        IERC20(asset).approve(address(POOL), amountOwed);

        uint256 profit = IERC20(asset).balanceOf(address(this));
        if (profit > 0) {
            IERC20(asset).transfer(owner(), profit);
        }

        emit LiquidationExecuted(user, debtAsset, amount, seized, profit);
        return true;
    }

    /**
     * @dev Update the executor address. Only callable by the owner.
     */
    function setExecutor(address newExecutor) external onlyOwner {
        require(newExecutor != address(0), "ZERO_EXECUTOR");
        address old = executor;
        executor = newExecutor;
        emit ExecutorUpdated(old, newExecutor);
    }

    /**
     * @dev Emergency withdrawal of tokens accidentally sent to the contract.
     * Only callable by the owner.
     */
    function rescueTokens(address token, uint256 amount, address to) external onlyOwner {
        require(to != address(0), "ZERO_RECIPIENT");
        IERC20(token).transfer(to, amount);
        emit TokenRescued(token, amount, to);
    }
}
