/// Whether [balance] covers [amount], including an exact match.
bool canWithdraw(int balance, int amount) => balance >= amount;
