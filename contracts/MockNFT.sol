// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @notice Minimal local-only contract for exercising the bot with Anvil.
contract MockNFT {
    bool public publicSaleActive;
    uint256 public salePhase;
    uint256 public totalMinted;

    event PublicSaleStarted();
    event Minted(address indexed minter, uint256 quantity);

    function setPublicSale(bool active) external {
        publicSaleActive = active;
        salePhase = active ? 2 : 0;
        if (active) {
            emit PublicSaleStarted();
        }
    }

    function mint(uint256 quantity) external payable {
        require(publicSaleActive, "sale closed");
        require(quantity > 0, "quantity is zero");
        totalMinted += quantity;
        emit Minted(msg.sender, quantity);
    }
}
